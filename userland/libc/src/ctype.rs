#[no_mangle]
pub extern "C" fn isalnum(c: i32) -> i32 {
    (isalpha(c) != 0 || isdigit(c) != 0) as i32
}

#[no_mangle]
pub extern "C" fn isalpha(c: i32) -> i32 {
    (isupper(c) != 0 || islower(c) != 0) as i32
}

#[no_mangle]
pub extern "C" fn isdigit(c: i32) -> i32 {
    (c >= b'0' as i32 && c <= b'9' as i32) as i32
}

#[no_mangle]
pub extern "C" fn islower(c: i32) -> i32 {
    (c >= b'a' as i32 && c <= b'z' as i32) as i32
}

#[no_mangle]
pub extern "C" fn isupper(c: i32) -> i32 {
    (c >= b'A' as i32 && c <= b'Z' as i32) as i32
}

#[no_mangle]
pub extern "C" fn isprint(c: i32) -> i32 {
    (c >= 0x20 && c <= 0x7e) as i32
}

#[no_mangle]
pub extern "C" fn isspace(c: i32) -> i32 {
    (c == b' ' as i32 || c == b'\t' as i32 || c == b'\n' as i32
        || c == b'\r' as i32 || c == 0x0b || c == 0x0c) as i32
}

#[no_mangle]
pub extern "C" fn isxdigit(c: i32) -> i32 {
    (isdigit(c) != 0
        || (c >= b'a' as i32 && c <= b'f' as i32)
        || (c >= b'A' as i32 && c <= b'F' as i32)) as i32
}

#[no_mangle]
pub extern "C" fn toupper(c: i32) -> i32 {
    if islower(c) != 0 { c - 32 } else { c }
}

#[no_mangle]
pub extern "C" fn tolower(c: i32) -> i32 {
    if isupper(c) != 0 { c + 32 } else { c }
}

#[inline(never)]
pub fn _libc_ctype_init() {}
