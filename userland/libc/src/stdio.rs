use std::io::{Read, Seek, SeekFrom, Write};
use std::ptr;

/// FILE backed by std I/O — works on both ToyOS and the host.
pub struct FILE {
    kind: FileKind,
    eof: bool,
    error: bool,
}

enum FileKind {
    Stdout,
    Stderr,
    Stdin,
    Owned(std::fs::File),
}

static mut STDOUT_FILE: FILE = FILE { kind: FileKind::Stdout, eof: false, error: false };
static mut STDERR_FILE: FILE = FILE { kind: FileKind::Stderr, eof: false, error: false };
static mut STDIN_FILE: FILE = FILE { kind: FileKind::Stdin, eof: false, error: false };

#[no_mangle]
pub static mut stdout: *mut FILE = &raw mut STDOUT_FILE;
#[no_mangle]
pub static mut stderr: *mut FILE = &raw mut STDERR_FILE;
#[no_mangle]
pub static mut stdin: *mut FILE = &raw mut STDIN_FILE;

#[no_mangle]
pub unsafe extern "C" fn fopen(path: *const u8, mode: *const u8) -> *mut FILE {
    let path_len = super::string::strlen(path);
    let path_str = core::str::from_utf8_unchecked(core::slice::from_raw_parts(path, path_len));

    let file = match *mode {
        b'r' => {
            let mut opts = std::fs::OpenOptions::new();
            opts.read(true);
            if *mode.add(1) == b'+' || (*mode.add(1) != 0 && *mode.add(2) == b'+') {
                opts.write(true);
            }
            opts.open(path_str)
        }
        b'w' => {
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            if *mode.add(1) == b'+' || (*mode.add(1) != 0 && *mode.add(2) == b'+') {
                opts.read(true);
            }
            opts.open(path_str)
        }
        b'a' => {
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create(true).append(true);
            if *mode.add(1) == b'+' || (*mode.add(1) != 0 && *mode.add(2) == b'+') {
                opts.read(true);
            }
            opts.open(path_str)
        }
        _ => return ptr::null_mut(),
    };

    let file = match file {
        Ok(f) => f,
        Err(_) => return ptr::null_mut(),
    };

    let f = super::memory::malloc(core::mem::size_of::<FILE>()) as *mut FILE;
    if f.is_null() {
        return ptr::null_mut();
    }
    ptr::write(f, FILE { kind: FileKind::Owned(file), eof: false, error: false });
    f
}

#[no_mangle]
pub unsafe extern "C" fn fclose(f: *mut FILE) -> i32 {
    if f.is_null() || f == stdout || f == stderr || f == stdin {
        return -1;
    }
    ptr::drop_in_place(f);
    super::memory::free(f as *mut u8);
    0
}

#[no_mangle]
pub unsafe extern "C" fn fread(buf: *mut u8, size: usize, count: usize, f: *mut FILE) -> usize {
    if f.is_null() || size == 0 || count == 0 {
        return 0;
    }
    let total = size * count;
    let slice = core::slice::from_raw_parts_mut(buf, total);
    let mut read_so_far = 0;
    while read_so_far < total {
        let result = match &mut (*f).kind {
            FileKind::Stdin => std::io::stdin().read(&mut slice[read_so_far..]),
            FileKind::Owned(file) => file.read(&mut slice[read_so_far..]),
            _ => return read_so_far / size,
        };
        match result {
            Ok(0) => { (*f).eof = true; break; }
            Ok(n) => read_so_far += n,
            Err(_) => { (*f).error = true; break; }
        }
    }
    read_so_far / size
}

#[no_mangle]
pub unsafe extern "C" fn fwrite(buf: *const u8, size: usize, count: usize, f: *mut FILE) -> usize {
    if f.is_null() || size == 0 || count == 0 {
        return 0;
    }
    let total = size * count;
    let slice = core::slice::from_raw_parts(buf, total);
    let result = match &mut (*f).kind {
        FileKind::Stdout => std::io::stdout().write_all(slice),
        FileKind::Stderr => std::io::stderr().write_all(slice),
        FileKind::Owned(file) => file.write_all(slice),
        FileKind::Stdin => return 0,
    };
    match result {
        Ok(()) => count,
        Err(_) => { (*f).error = true; 0 }
    }
}

#[no_mangle]
pub unsafe extern "C" fn fseek(f: *mut FILE, offset: i64, whence: i32) -> i32 {
    if f.is_null() { return -1; }
    (*f).eof = false;
    let pos = match whence {
        0 => SeekFrom::Start(offset as u64),
        1 => SeekFrom::Current(offset),
        2 => SeekFrom::End(offset),
        _ => return -1,
    };
    match &mut (*f).kind {
        FileKind::Owned(file) => if file.seek(pos).is_ok() { 0 } else { -1 },
        _ => -1,
    }
}

#[no_mangle]
pub unsafe extern "C" fn ftell(f: *mut FILE) -> i64 {
    if f.is_null() { return -1; }
    match &mut (*f).kind {
        FileKind::Owned(file) => file.stream_position().map_or(-1, |p| p as i64),
        _ => -1,
    }
}

#[no_mangle]
pub unsafe extern "C" fn rewind(f: *mut FILE) {
    if !f.is_null() {
        fseek(f, 0, 0);
        (*f).error = false;
    }
}

#[no_mangle]
pub unsafe extern "C" fn feof(f: *mut FILE) -> i32 {
    if f.is_null() { return 0; }
    (*f).eof as i32
}

#[no_mangle]
pub unsafe extern "C" fn ferror(f: *mut FILE) -> i32 {
    if f.is_null() { return 0; }
    (*f).error as i32
}

#[no_mangle]
pub unsafe extern "C" fn clearerr(f: *mut FILE) {
    if !f.is_null() {
        (*f).eof = false;
        (*f).error = false;
    }
}

#[no_mangle]
pub unsafe extern "C" fn fflush(_f: *mut FILE) -> i32 {
    0
}

#[no_mangle]
pub unsafe extern "C" fn fileno(_f: *mut FILE) -> i32 {
    -1 // no raw fd access
}

#[no_mangle]
pub unsafe extern "C" fn fgetc(f: *mut FILE) -> i32 {
    let mut c: u8 = 0;
    if fread(&mut c as *mut u8, 1, 1, f) == 1 {
        c as i32
    } else {
        -1 // EOF
    }
}

#[no_mangle]
pub unsafe extern "C" fn fputc(c: i32, f: *mut FILE) -> i32 {
    let b = c as u8;
    if fwrite(&b as *const u8, 1, 1, f) == 1 {
        c
    } else {
        -1
    }
}

#[no_mangle]
pub unsafe extern "C" fn fgets(buf: *mut u8, n: i32, f: *mut FILE) -> *mut u8 {
    if n <= 0 { return ptr::null_mut(); }
    let mut i = 0;
    while i < (n - 1) as usize {
        let c = fgetc(f);
        if c == -1 {
            if i == 0 { return ptr::null_mut(); }
            break;
        }
        *buf.add(i) = c as u8;
        i += 1;
        if c == b'\n' as i32 { break; }
    }
    *buf.add(i) = 0;
    buf
}

#[no_mangle]
pub unsafe extern "C" fn fputs(s: *const u8, f: *mut FILE) -> i32 {
    let len = super::string::strlen(s);
    fwrite(s, 1, len, f);
    0
}

#[no_mangle]
pub unsafe extern "C" fn getc(f: *mut FILE) -> i32 {
    fgetc(f)
}

#[no_mangle]
pub unsafe extern "C" fn putc(c: i32, f: *mut FILE) -> i32 {
    fputc(c, f)
}

#[no_mangle]
pub unsafe extern "C" fn getchar() -> i32 {
    fgetc(stdin)
}

#[no_mangle]
pub unsafe extern "C" fn putchar(c: i32) -> i32 {
    fputc(c, stdout)
}

#[no_mangle]
pub unsafe extern "C" fn puts(s: *const u8) -> i32 {
    fputs(s, stdout);
    fputc(b'\n' as i32, stdout);
    0
}

#[no_mangle]
pub unsafe extern "C" fn ungetc(_c: i32, _f: *mut FILE) -> i32 {
    -1 // Not implemented
}

#[no_mangle]
pub unsafe extern "C" fn remove(path: *const u8) -> i32 {
    let path_len = super::string::strlen(path);
    let path_str = core::str::from_utf8_unchecked(core::slice::from_raw_parts(path, path_len));
    if std::fs::remove_file(path_str).is_ok() { 0 } else { -1 }
}

#[no_mangle]
pub unsafe extern "C" fn rename(old: *const u8, new: *const u8) -> i32 {
    let old_len = super::string::strlen(old);
    let new_len = super::string::strlen(new);
    let old_str = core::str::from_utf8_unchecked(core::slice::from_raw_parts(old, old_len));
    let new_str = core::str::from_utf8_unchecked(core::slice::from_raw_parts(new, new_len));
    if std::fs::rename(old_str, new_str).is_ok() { 0 } else { -1 }
}

#[no_mangle]
pub unsafe extern "C" fn tmpfile() -> *mut FILE {
    ptr::null_mut()
}

#[no_mangle]
pub unsafe extern "C" fn perror(s: *const u8) {
    if !s.is_null() && *s != 0 {
        fputs(s, stderr);
        fputs(b": \0".as_ptr(), stderr);
    }
    fputs(b"error\n\0".as_ptr(), stderr);
}

#[no_mangle]
pub unsafe extern "C" fn fdopen(_fd: i32, _mode: *const u8) -> *mut FILE {
    ptr::null_mut() // no raw fd support
}

// Misc stdlib functions

#[no_mangle]
pub unsafe extern "C" fn exit(code: i32) -> ! {
    std::process::exit(code)
}

#[no_mangle]
pub unsafe extern "C" fn _exit(code: i32) -> ! {
    std::process::exit(code)
}

#[no_mangle]
pub unsafe extern "C" fn abort() -> ! {
    std::process::exit(134)
}

#[no_mangle]
pub unsafe extern "C" fn atexit(_func: extern "C" fn()) -> i32 {
    0 // stub
}

#[no_mangle]
pub unsafe extern "C" fn atoi(s: *const u8) -> i32 {
    super::string::strtol(s, ptr::null_mut(), 10) as i32
}

#[no_mangle]
pub unsafe extern "C" fn atol(s: *const u8) -> i64 {
    super::string::strtol(s, ptr::null_mut(), 10)
}

#[no_mangle]
pub unsafe extern "C" fn atof(s: *const u8) -> f64 {
    let mut p = s;
    while super::ctype::isspace(*p as i32) != 0 { p = p.add(1); }
    // Find the end of the numeric portion (sign, digits, dot, exponent)
    let start = p;
    if *p == b'-' || *p == b'+' { p = p.add(1); }
    while (*p).is_ascii_digit() { p = p.add(1); }
    if *p == b'.' { p = p.add(1); while (*p).is_ascii_digit() { p = p.add(1); } }
    if *p == b'e' || *p == b'E' {
        p = p.add(1);
        if *p == b'-' || *p == b'+' { p = p.add(1); }
        while (*p).is_ascii_digit() { p = p.add(1); }
    }
    let len = p as usize - start as usize;
    let slice = core::slice::from_raw_parts(start, len);
    core::str::from_utf8_unchecked(slice).parse::<f64>().unwrap_or(0.0)
}

#[no_mangle]
pub unsafe extern "C" fn getenv(_name: *const u8) -> *const u8 {
    ptr::null()
}

#[no_mangle]
pub unsafe extern "C" fn setenv(_name: *const u8, _value: *const u8, _overwrite: i32) -> i32 {
    -1
}

#[no_mangle]
pub unsafe extern "C" fn unsetenv(_name: *const u8) -> i32 {
    -1
}

#[no_mangle]
pub unsafe extern "C" fn system(_command: *const u8) -> i32 {
    -1 // no shell available
}

#[no_mangle]
pub extern "C" fn abs(n: i32) -> i32 {
    if n < 0 { -n } else { n }
}

#[no_mangle]
pub extern "C" fn labs(n: i64) -> i64 {
    if n < 0 { -n } else { n }
}

#[no_mangle]
pub unsafe extern "C" fn qsort(
    base: *mut u8,
    count: usize,
    size: usize,
    cmp: extern "C" fn(*const u8, *const u8) -> i32,
) {
    if count <= 1 { return; }
    let tmp = super::memory::malloc(size);
    for i in 1..count {
        let mut j = i;
        while j > 0 && cmp(base.add(j * size), base.add((j - 1) * size)) < 0 {
            ptr::copy_nonoverlapping(base.add(j * size), tmp, size);
            ptr::copy_nonoverlapping(base.add((j - 1) * size), base.add(j * size), size);
            ptr::copy_nonoverlapping(tmp, base.add((j - 1) * size), size);
            j -= 1;
        }
    }
    super::memory::free(tmp);
}

#[no_mangle]
pub unsafe extern "C" fn bsearch(
    key: *const u8,
    base: *const u8,
    count: usize,
    size: usize,
    cmp: extern "C" fn(*const u8, *const u8) -> i32,
) -> *mut u8 {
    let mut lo = 0usize;
    let mut hi = count;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let elem = base.add(mid * size);
        let c = cmp(key, elem);
        if c == 0 { return elem as *mut u8; }
        if c < 0 { hi = mid; } else { lo = mid + 1; }
    }
    ptr::null_mut()
}

static mut RAND_SEED: u32 = 1;

#[no_mangle]
pub unsafe extern "C" fn rand() -> i32 {
    RAND_SEED = RAND_SEED.wrapping_mul(1103515245).wrapping_add(12345);
    (RAND_SEED >> 16) as i32 & 0x7fff
}

#[no_mangle]
pub unsafe extern "C" fn srand(seed: u32) {
    RAND_SEED = seed;
}

#[no_mangle]
pub unsafe extern "C" fn mkdir(path: *const u8, _mode: u32) -> i32 {
    let path_len = super::string::strlen(path);
    let path_str = core::str::from_utf8_unchecked(core::slice::from_raw_parts(path, path_len));
    if std::fs::create_dir(path_str).is_ok() { 0 } else { -1 }
}

#[no_mangle]
pub unsafe extern "C" fn stat(_path: *const u8, _buf: *mut u8) -> i32 {
    -1 // Not implemented
}

#[no_mangle]
pub unsafe extern "C" fn __assert_fail(expr: *const u8, file: *const u8, line: i32) {
    let expr_s = core::str::from_utf8_unchecked(core::slice::from_raw_parts(expr, super::string::strlen(expr)));
    let file_s = core::str::from_utf8_unchecked(core::slice::from_raw_parts(file, super::string::strlen(file)));
    panic!("assertion failed: {expr_s} at {file_s}:{line}");
}

#[no_mangle]
pub static mut errno: i32 = 0;

#[inline(never)]
pub fn _libc_stdio_init() {}
