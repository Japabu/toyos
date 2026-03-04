use std::ptr;

#[no_mangle]
pub unsafe extern "C" fn strlen(s: *const u8) -> usize {
    let mut len = 0;
    while *s.add(len) != 0 {
        len += 1;
    }
    len
}

#[no_mangle]
pub unsafe extern "C" fn strcpy(dst: *mut u8, src: *const u8) -> *mut u8 {
    let mut i = 0;
    loop {
        *dst.add(i) = *src.add(i);
        if *src.add(i) == 0 {
            break;
        }
        i += 1;
    }
    dst
}

#[no_mangle]
pub unsafe extern "C" fn strncpy(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    let mut i = 0;
    while i < n && *src.add(i) != 0 {
        *dst.add(i) = *src.add(i);
        i += 1;
    }
    while i < n {
        *dst.add(i) = 0;
        i += 1;
    }
    dst
}

#[no_mangle]
pub unsafe extern "C" fn strcat(dst: *mut u8, src: *const u8) -> *mut u8 {
    let end = dst.add(strlen(dst));
    strcpy(end, src);
    dst
}

#[no_mangle]
pub unsafe extern "C" fn strncat(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    let end = dst.add(strlen(dst));
    let mut i = 0;
    while i < n && *src.add(i) != 0 {
        *end.add(i) = *src.add(i);
        i += 1;
    }
    *end.add(i) = 0;
    dst
}

#[no_mangle]
pub unsafe extern "C" fn strcmp(a: *const u8, b: *const u8) -> i32 {
    let mut i = 0;
    loop {
        let ca = *a.add(i);
        let cb = *b.add(i);
        if ca != cb || ca == 0 {
            return ca as i32 - cb as i32;
        }
        i += 1;
    }
}

#[no_mangle]
pub unsafe extern "C" fn strncmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    for i in 0..n {
        let ca = *a.add(i);
        let cb = *b.add(i);
        if ca != cb || ca == 0 {
            return ca as i32 - cb as i32;
        }
    }
    0
}

fn to_lower(c: u8) -> u8 {
    if c.is_ascii_uppercase() { c + 32 } else { c }
}

#[no_mangle]
pub unsafe extern "C" fn strcasecmp(a: *const u8, b: *const u8) -> i32 {
    let mut i = 0;
    loop {
        let ca = to_lower(*a.add(i));
        let cb = to_lower(*b.add(i));
        if ca != cb || ca == 0 {
            return ca as i32 - cb as i32;
        }
        i += 1;
    }
}

#[no_mangle]
pub unsafe extern "C" fn strncasecmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    for i in 0..n {
        let ca = to_lower(*a.add(i));
        let cb = to_lower(*b.add(i));
        if ca != cb || ca == 0 {
            return ca as i32 - cb as i32;
        }
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn strchr(s: *const u8, c: i32) -> *mut u8 {
    let c = c as u8;
    let mut p = s;
    loop {
        if *p == c {
            return p as *mut u8;
        }
        if *p == 0 {
            return ptr::null_mut();
        }
        p = p.add(1);
    }
}

#[no_mangle]
pub unsafe extern "C" fn strrchr(s: *const u8, c: i32) -> *mut u8 {
    let c = c as u8;
    let mut last = ptr::null_mut();
    let mut p = s;
    loop {
        if *p == c {
            last = p as *mut u8;
        }
        if *p == 0 {
            return last;
        }
        p = p.add(1);
    }
}

#[no_mangle]
pub unsafe extern "C" fn strstr(haystack: *const u8, needle: *const u8) -> *mut u8 {
    if *needle == 0 {
        return haystack as *mut u8;
    }
    let nlen = strlen(needle);
    let hlen = strlen(haystack);
    if nlen > hlen {
        return ptr::null_mut();
    }
    for i in 0..=(hlen - nlen) {
        if strncmp(haystack.add(i), needle, nlen) == 0 {
            return haystack.add(i) as *mut u8;
        }
    }
    ptr::null_mut()
}

#[no_mangle]
pub unsafe extern "C" fn strdup(s: *const u8) -> *mut u8 {
    let len = strlen(s);
    let p = super::memory::malloc(len + 1);
    if !p.is_null() {
        ptr::copy_nonoverlapping(s, p, len + 1);
    }
    p
}

#[no_mangle]
pub unsafe extern "C" fn strtol(s: *const u8, endptr: *mut *mut u8, base: i32) -> i64 {
    let mut p = s;
    // Skip whitespace
    while *p == b' ' || *p == b'\t' || *p == b'\n' || *p == b'\r' {
        p = p.add(1);
    }
    let negative = *p == b'-';
    if *p == b'-' || *p == b'+' {
        p = p.add(1);
    }
    let base = if base == 0 {
        if *p == b'0' {
            p = p.add(1);
            if *p == b'x' || *p == b'X' {
                p = p.add(1);
                16
            } else {
                8
            }
        } else {
            10
        }
    } else {
        if base == 16 && *p == b'0' && (*p.add(1) == b'x' || *p.add(1) == b'X') {
            p = p.add(2);
        }
        base as u32
    };
    let mut result: i64 = 0;
    loop {
        let digit = match *p {
            b'0'..=b'9' => (*p - b'0') as u32,
            b'a'..=b'z' => (*p - b'a' + 10) as u32,
            b'A'..=b'Z' => (*p - b'A' + 10) as u32,
            _ => break,
        };
        if digit >= base {
            break;
        }
        result = result.wrapping_mul(base as i64).wrapping_add(digit as i64);
        p = p.add(1);
    }
    if !endptr.is_null() {
        *endptr = p as *mut u8;
    }
    if negative { -result } else { result }
}

#[no_mangle]
pub unsafe extern "C" fn strtoul(s: *const u8, endptr: *mut *mut u8, base: i32) -> u64 {
    let mut p = s;
    while *p == b' ' || *p == b'\t' || *p == b'\n' || *p == b'\r' {
        p = p.add(1);
    }
    // strtoul accepts optional sign per C spec
    let negative = *p == b'-';
    if *p == b'-' || *p == b'+' {
        p = p.add(1);
    }
    let base = if base == 0 {
        if *p == b'0' {
            p = p.add(1);
            if *p == b'x' || *p == b'X' { p = p.add(1); 16 } else { 8 }
        } else {
            10
        }
    } else {
        if base == 16 && *p == b'0' && (*p.add(1) == b'x' || *p.add(1) == b'X') {
            p = p.add(2);
        }
        base as u64
    };
    let mut result: u64 = 0;
    loop {
        let digit = match *p {
            b'0'..=b'9' => (*p - b'0') as u64,
            b'a'..=b'z' => (*p - b'a' + 10) as u64,
            b'A'..=b'Z' => (*p - b'A' + 10) as u64,
            _ => break,
        };
        if digit >= base { break; }
        result = result.wrapping_mul(base).wrapping_add(digit);
        p = p.add(1);
    }
    if !endptr.is_null() {
        *endptr = p as *mut u8;
    }
    if negative { result.wrapping_neg() } else { result }
}

#[no_mangle]
pub unsafe extern "C" fn strerror(_errnum: i32) -> *const u8 {
    b"unknown error\0".as_ptr()
}

#[no_mangle]
pub unsafe extern "C" fn strspn(s: *const u8, accept: *const u8) -> usize {
    let mut count = 0;
    while *s.add(count) != 0 {
        if strchr(accept, *s.add(count) as i32).is_null() {
            break;
        }
        count += 1;
    }
    count
}

#[no_mangle]
pub unsafe extern "C" fn strcspn(s: *const u8, reject: *const u8) -> usize {
    let mut count = 0;
    while *s.add(count) != 0 {
        if !strchr(reject, *s.add(count) as i32).is_null() {
            break;
        }
        count += 1;
    }
    count
}

#[no_mangle]
pub unsafe extern "C" fn strpbrk(s: *const u8, accept: *const u8) -> *mut u8 {
    let mut p = s;
    while *p != 0 {
        if !strchr(accept, *p as i32).is_null() {
            return p as *mut u8;
        }
        p = p.add(1);
    }
    ptr::null_mut()
}

static mut STRTOK_POS: *mut u8 = ptr::null_mut();

#[no_mangle]
pub unsafe extern "C" fn strtok(s: *mut u8, delim: *const u8) -> *mut u8 {
    let p = if s.is_null() { STRTOK_POS } else { s };
    if p.is_null() || *p == 0 {
        return ptr::null_mut();
    }
    let start = p.add(strspn(p, delim));
    if *start == 0 {
        STRTOK_POS = ptr::null_mut();
        return ptr::null_mut();
    }
    let end = start.add(strcspn(start, delim));
    if *end != 0 {
        *end = 0;
        STRTOK_POS = end.add(1);
    } else {
        STRTOK_POS = ptr::null_mut();
    }
    start
}

#[inline(never)]
pub fn _libc_string_init() {}
