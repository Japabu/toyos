use std::ptr;
use toyos_abi::syscall;

/// Minimal FILE implementation backed by ToyOS file descriptors.
#[repr(C)]
pub struct FILE {
    fd: u64,
    eof: bool,
    error: bool,
}

static mut STDOUT_FILE: FILE = FILE { fd: 1, eof: false, error: false };
static mut STDERR_FILE: FILE = FILE { fd: 2, eof: false, error: false };

#[no_mangle]
pub static mut stdout: *mut FILE = &raw mut STDOUT_FILE;
#[no_mangle]
pub static mut stderr: *mut FILE = &raw mut STDERR_FILE;
#[no_mangle]
pub static mut stdin: *mut FILE = ptr::null_mut();

#[no_mangle]
pub unsafe extern "C" fn fopen(path: *const u8, mode: *const u8) -> *mut FILE {
    let path_len = super::string::strlen(path);
    let mode_str = mode;
    let mut flags: u64 = match *mode_str {
        b'r' => 0, // O_RDONLY
        b'w' => 1 | 0x40 | 0x200, // O_WRONLY | O_CREAT | O_TRUNC
        b'a' => 1 | 0x40 | 0x400, // O_WRONLY | O_CREAT | O_APPEND
        _ => return ptr::null_mut(),
    };
    if *mode_str.add(1) == b'+' || (*mode_str.add(1) != 0 && *mode_str.add(2) == b'+') {
        flags = (flags & !3) | 2; // O_RDWR
    }

    let fd = syscall::open(path, path_len, flags);
    if fd == u64::MAX {
        return ptr::null_mut();
    }

    let f = super::memory::malloc(core::mem::size_of::<FILE>()) as *mut FILE;
    if f.is_null() {
        syscall::close(fd);
        return ptr::null_mut();
    }
    ptr::write(f, FILE { fd, eof: false, error: false });
    f
}

#[no_mangle]
pub unsafe extern "C" fn fclose(f: *mut FILE) -> i32 {
    if f.is_null() || f == stdout || f == stderr {
        return -1;
    }
    syscall::close((*f).fd);
    super::memory::free(f as *mut u8);
    0
}

#[no_mangle]
pub unsafe extern "C" fn fread(buf: *mut u8, size: usize, count: usize, f: *mut FILE) -> usize {
    if f.is_null() || size == 0 || count == 0 {
        return 0;
    }
    let total = size * count;
    let mut read_so_far = 0;
    while read_so_far < total {
        let n = syscall::read((*f).fd, buf.add(read_so_far), total - read_so_far);
        if n == 0 || n == u64::MAX {
            if n == 0 { (*f).eof = true; }
            if n == u64::MAX { (*f).error = true; }
            break;
        }
        read_so_far += n as usize;
    }
    read_so_far / size
}

#[no_mangle]
pub unsafe extern "C" fn fwrite(buf: *const u8, size: usize, count: usize, f: *mut FILE) -> usize {
    if f.is_null() || size == 0 || count == 0 {
        return 0;
    }
    let total = size * count;
    let n = syscall::write((*f).fd, buf, total);
    if n == u64::MAX {
        (*f).error = true;
        return 0;
    }
    n as usize / size
}

#[no_mangle]
pub unsafe extern "C" fn fseek(f: *mut FILE, offset: i64, whence: i32) -> i32 {
    if f.is_null() { return -1; }
    (*f).eof = false;
    let result = syscall::seek((*f).fd, offset, whence as u64);
    if result == u64::MAX { -1 } else { 0 }
}

#[no_mangle]
pub unsafe extern "C" fn ftell(f: *mut FILE) -> i64 {
    if f.is_null() { return -1; }
    let pos = syscall::seek((*f).fd, 0, 1); // SEEK_CUR
    if pos == u64::MAX { -1 } else { pos as i64 }
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
pub unsafe extern "C" fn fileno(f: *mut FILE) -> i32 {
    if f.is_null() { return -1; }
    (*f).fd as i32
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
    let path_slice = core::slice::from_raw_parts(path, path_len);
    let path_str = core::str::from_utf8_unchecked(path_slice);
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
pub unsafe extern "C" fn fdopen(fd: i32, _mode: *const u8) -> *mut FILE {
    let f = super::memory::malloc(core::mem::size_of::<FILE>()) as *mut FILE;
    if f.is_null() { return ptr::null_mut(); }
    ptr::write(f, FILE { fd: fd as u64, eof: false, error: false });
    f
}

// Misc stdlib functions that use stdio or syscalls

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
    // Simple insertion sort (good enough for DOOM's small arrays)
    if count <= 1 { return; }
    let tmp = super::memory::malloc(size);
    for i in 1..count {
        let mut j = i;
        while j > 0 && cmp(base.add(j * size), base.add((j - 1) * size)) < 0 {
            // Swap elements j and j-1
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
