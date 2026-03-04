use core::ffi::VaList;
use core::fmt::Write;

/// Output buffer for printf family. Writes to a raw C buffer with optional capacity limit.
struct BufWriter {
    buf: *mut u8,
    pos: usize,
    cap: usize, // usize::MAX = unlimited (sprintf)
}

impl Write for BufWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for &b in s.as_bytes() {
            if self.cap > 0 && self.pos >= self.cap - 1 {
                continue; // leave room for null terminator
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

/// Write an integer with optional precision (minimum digits, zero-padded).
fn write_int_padded(
    w: &mut BufWriter, s: &str, prefix_len: usize,
    width: usize, pad_char: char, left_align: bool, precision: Option<usize>,
) {
    if let Some(prec) = precision {
        let digit_len = s.len() - prefix_len;
        if digit_len < prec {
            let zeros = prec - digit_len;
            let mut padded = vec![0u8; prefix_len + zeros + digit_len];
            let (mut p, bytes) = (0, s.as_bytes());
            for &b in &bytes[..prefix_len] { padded[p] = b; p += 1; }
            for _ in 0..zeros { padded[p] = b'0'; p += 1; }
            for &b in &bytes[prefix_len..] { padded[p] = b; p += 1; }
            // SAFETY: padded contains only ASCII digits and sign characters
            let s2 = unsafe { core::str::from_utf8_unchecked(&padded[..p]) };
            write_padded(w, s2, width, ' ', left_align);
            return;
        }
        write_padded(w, s, width, ' ', left_align);
    } else {
        write_padded(w, s, width, pad_char, left_align);
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
        pos -= 1; buf[pos] = b'-';
    } else if plus {
        pos -= 1; buf[pos] = b'+';
    } else if space {
        pos -= 1; buf[pos] = b' ';
    }
    // SAFETY: buf contains only ASCII
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
    // SAFETY: buf contains only ASCII hex digits
    unsafe { core::str::from_utf8_unchecked(&buf[pos..]) }
}

/// Core printf engine. Parses the format string as a byte slice.
unsafe fn do_printf(buf: *mut u8, n: usize, fmt: *const u8, ap: &mut VaList<'_>) -> i32 {
    let fmt = core::slice::from_raw_parts(fmt, super::string::strlen(fmt));
    let mut w = BufWriter { buf, pos: 0, cap: n };
    let mut i = 0;

    while i < fmt.len() {
        if fmt[i] != b'%' {
            let _ = w.write_char(fmt[i] as char);
            i += 1;
            continue;
        }
        i += 1;

        // Flags
        let mut left_align = false;
        let mut zero_pad = false;
        let mut plus_sign = false;
        let mut space_sign = false;
        while i < fmt.len() {
            match fmt[i] {
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
        if i < fmt.len() && fmt[i] == b'*' {
            let w = ap.arg::<i32>();
            if w < 0 { left_align = true; width = (-w) as usize; } else { width = w as usize; }
            i += 1;
        } else {
            while i < fmt.len() && fmt[i].is_ascii_digit() {
                width = width * 10 + (fmt[i] - b'0') as usize;
                i += 1;
            }
        }

        // Precision
        let mut precision: Option<usize> = None;
        if i < fmt.len() && fmt[i] == b'.' {
            i += 1;
            let mut prec = 0;
            if i < fmt.len() && fmt[i] == b'*' {
                prec = ap.arg::<i32>().max(0) as usize;
                i += 1;
            } else {
                while i < fmt.len() && fmt[i].is_ascii_digit() {
                    prec = prec * 10 + (fmt[i] - b'0') as usize;
                    i += 1;
                }
            }
            precision = Some(prec);
        }

        if i >= fmt.len() { break; }

        // Length modifier
        let mut long = false;
        let mut long_long = false;
        match fmt[i] {
            b'l' => {
                i += 1;
                if i < fmt.len() && fmt[i] == b'l' { long_long = true; i += 1; } else { long = true; }
            }
            b'h' => { i += 1; if i < fmt.len() && fmt[i] == b'h' { i += 1; } }
            b'z' | b't' | b'j' => { long = true; i += 1; }
            _ => {}
        }

        if i >= fmt.len() { break; }

        let pad_char = if zero_pad && !left_align { '0' } else { ' ' };
        match fmt[i] {
            b'd' | b'i' => {
                let val: i64 = if long_long || long { ap.arg::<i64>() } else { ap.arg::<i32>() as i64 };
                let mut tmp = [0u8; 24];
                let s = format_signed(val, &mut tmp, plus_sign, space_sign);
                let prefix = s.len() - s.trim_start_matches(|c: char| !c.is_ascii_digit()).len();
                write_int_padded(&mut w, s, prefix, width, pad_char, left_align, precision);
            }
            b'u' => {
                let val: u64 = if long_long || long { ap.arg::<u64>() } else { ap.arg::<u32>() as u64 };
                let mut tmp = [0u8; 24];
                let s = format_unsigned(val, 10, false, &mut tmp);
                write_int_padded(&mut w, s, 0, width, pad_char, left_align, precision);
            }
            b'x' => {
                let val: u64 = if long_long || long { ap.arg::<u64>() } else { ap.arg::<u32>() as u64 };
                let mut tmp = [0u8; 20];
                let s = format_unsigned(val, 16, false, &mut tmp);
                write_int_padded(&mut w, s, 0, width, pad_char, left_align, precision);
            }
            b'X' => {
                let val: u64 = if long_long || long { ap.arg::<u64>() } else { ap.arg::<u32>() as u64 };
                let mut tmp = [0u8; 20];
                let s = format_unsigned(val, 16, true, &mut tmp);
                write_int_padded(&mut w, s, 0, width, pad_char, left_align, precision);
            }
            b'o' => {
                let val: u64 = if long_long || long { ap.arg::<u64>() } else { ap.arg::<u32>() as u64 };
                let mut tmp = [0u8; 24];
                let s = format_unsigned(val, 8, false, &mut tmp);
                write_int_padded(&mut w, s, 0, width, pad_char, left_align, precision);
            }
            b'c' => {
                let c = ap.arg::<i32>() as u8;
                let s = core::str::from_utf8_unchecked(core::slice::from_ref(&c));
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
                let mut hex = [0u8; 20];
                let hex_s = format_unsigned(p as u64, 16, false, &mut hex);
                let mut tmp = [0u8; 22];
                tmp[0] = b'0';
                tmp[1] = b'x';
                let len = hex_s.len();
                tmp[2..2 + len].copy_from_slice(hex_s.as_bytes());
                let s = core::str::from_utf8_unchecked(&tmp[..2 + len]);
                write_padded(&mut w, s, width, ' ', left_align);
            }
            b'%' => { let _ = w.write_char('%'); }
            other => {
                let _ = w.write_char('%');
                let _ = w.write_char(other as char);
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
pub unsafe extern "C" fn vprintf(fmt: *const u8, ap: VaList<'_>) -> i32 {
    vfprintf(super::stdio::stdout, fmt, ap)
}

#[no_mangle]
pub unsafe extern "C" fn vsprintf(buf: *mut u8, fmt: *const u8, ap: VaList<'_>) -> i32 {
    vsnprintf(buf, usize::MAX, fmt, ap)
}

#[no_mangle]
pub unsafe extern "C" fn sscanf(input: *const u8, fmt: *const u8, mut args: ...) -> i32 {
    let input = core::slice::from_raw_parts(input, super::string::strlen(input));
    let fmt = core::slice::from_raw_parts(fmt, super::string::strlen(fmt));
    let mut si = 0usize;
    let mut fi = 0usize;
    let mut matched = 0i32;

    while fi < fmt.len() && si < input.len() {
        if fmt[fi] == b'%' {
            fi += 1;
            if fi >= fmt.len() { break; }
            match fmt[fi] {
                b'd' => {
                    let p: *mut i32 = args.arg::<*mut i32>();
                    while si < input.len() && (input[si] as char).is_ascii_whitespace() { si += 1; }
                    let mut endptr: *mut u8 = std::ptr::null_mut();
                    let val = super::string::strtol(input.as_ptr().add(si), &mut endptr, 10);
                    let consumed = endptr as usize - input.as_ptr().add(si) as usize;
                    if consumed == 0 { break; }
                    *p = val as i32;
                    si += consumed;
                    matched += 1;
                }
                b's' => {
                    let p: *mut u8 = args.arg::<*mut u8>();
                    while si < input.len() && (input[si] as char).is_ascii_whitespace() { si += 1; }
                    let mut j = 0;
                    while si < input.len() && !(input[si] as char).is_ascii_whitespace() {
                        *p.add(j) = input[si];
                        j += 1;
                        si += 1;
                    }
                    *p.add(j) = 0;
                    matched += 1;
                }
                _ => break,
            }
            fi += 1;
        } else if (fmt[fi] as char).is_ascii_whitespace() {
            while si < input.len() && (input[si] as char).is_ascii_whitespace() { si += 1; }
            fi += 1;
        } else {
            if input[si] != fmt[fi] { break; }
            si += 1;
            fi += 1;
        }
    }
    matched
}

#[inline(never)]
pub fn _libc_printf_init() {}
