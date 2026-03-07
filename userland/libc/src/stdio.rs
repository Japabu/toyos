use core::ptr;
use toyos_abi::Fd;
use toyos_abi::syscall::{self, OpenFlags, SeekFrom};

// ---------------------------------------------------------------------------
// Platform fd operations
// ---------------------------------------------------------------------------

fn sys_open(path: &[u8], read: bool, write: bool, create: bool, truncate: bool, append: bool) -> i32 {
    let mut flags = OpenFlags(0);
    if read { flags |= OpenFlags::READ; }
    if write { flags |= OpenFlags::WRITE; }
    if create { flags |= OpenFlags::CREATE; }
    if truncate { flags |= OpenFlags::TRUNCATE; }
    if append { flags |= OpenFlags::APPEND; }
    match syscall::open(path, flags) {
        Ok(fd) => fd.0 as i32,
        Err(_) => -1,
    }
}

fn sys_close(fd: i32) { syscall::close(Fd(fd)); }

fn sys_read(fd: i32, buf: &mut [u8]) -> isize {
    match syscall::read(Fd(fd), buf) {
        Ok(n) => n as isize,
        Err(_) => -1,
    }
}

fn sys_write(fd: i32, buf: &[u8]) -> isize {
    match syscall::write(Fd(fd), buf) {
        Ok(n) => n as isize,
        Err(_) => -1,
    }
}

fn sys_seek(fd: i32, offset: i64, whence: i32) -> i64 {
    let pos = match whence {
        0 => SeekFrom::Start(offset as u64),
        1 => SeekFrom::Current(offset),
        2 => SeekFrom::End(offset),
        _ => return -1,
    };
    match syscall::seek(Fd(fd), pos) {
        Ok(n) => n as i64,
        Err(_) => -1,
    }
}

fn sys_fsync(fd: i32) { let _ = syscall::fsync(Fd(fd)); }

fn sys_delete(path: &[u8]) -> i32 {
    match syscall::delete(path) { Ok(()) => 0, Err(_) => -1 }
}

fn sys_rename(old: &[u8], new: &[u8]) -> i32 {
    match syscall::rename(old, new) { Ok(()) => 0, Err(_) => -1 }
}

fn sys_mkdir(path: &[u8]) -> i32 {
    match syscall::mkdir(path) { Ok(()) => 0, Err(_) => -1 }
}

// ---------------------------------------------------------------------------
// FILE struct
// ---------------------------------------------------------------------------

pub struct FILE {
    fd: i32,
    eof: bool,
    error: bool,
}

const STDIN_FD: i32 = 0;
const STDOUT_FD: i32 = 1;
const STDERR_FD: i32 = 2;

static mut STDOUT_FILE: FILE = FILE { fd: STDOUT_FD, eof: false, error: false };
static mut STDERR_FILE: FILE = FILE { fd: STDERR_FD, eof: false, error: false };
static mut STDIN_FILE: FILE = FILE { fd: STDIN_FD, eof: false, error: false };

#[no_mangle]
pub static mut stdout: *mut FILE = &raw mut STDOUT_FILE;
#[no_mangle]
pub static mut stderr: *mut FILE = &raw mut STDERR_FILE;
#[no_mangle]
pub static mut stdin: *mut FILE = &raw mut STDIN_FILE;

// ---------------------------------------------------------------------------
// FILE I/O
// ---------------------------------------------------------------------------

unsafe fn c_str_bytes(s: *const u8) -> &'static [u8] {
    let len = super::string::strlen(s);
    unsafe { core::slice::from_raw_parts(s, len) }
}

#[no_mangle]
pub unsafe extern "C" fn fopen(path: *const u8, mode: *const u8) -> *mut FILE {
    let path_bytes = unsafe { c_str_bytes(path) };

    let (read, write, create, truncate, append) = match unsafe { *mode } {
        b'r' => {
            let plus = unsafe { *mode.add(1) == b'+' || (*mode.add(1) != 0 && *mode.add(2) == b'+') };
            (true, plus, false, false, false)
        }
        b'w' => {
            let plus = unsafe { *mode.add(1) == b'+' || (*mode.add(1) != 0 && *mode.add(2) == b'+') };
            (plus, true, true, true, false)
        }
        b'a' => {
            let plus = unsafe { *mode.add(1) == b'+' || (*mode.add(1) != 0 && *mode.add(2) == b'+') };
            (plus, true, true, false, true)
        }
        _ => return ptr::null_mut(),
    };

    let fd = sys_open(path_bytes, read, write, create, truncate, append);
    if fd < 0 {
        return ptr::null_mut();
    }

    let f = super::memory::malloc(core::mem::size_of::<FILE>()) as *mut FILE;
    if f.is_null() {
        sys_close(fd);
        return ptr::null_mut();
    }
    unsafe { ptr::write(f, FILE { fd, eof: false, error: false }); }
    f
}

#[no_mangle]
pub unsafe extern "C" fn fclose(f: *mut FILE) -> i32 {
    if f.is_null() || f == unsafe { stdout } || f == unsafe { stderr } || f == unsafe { stdin } {
        return -1;
    }
    sys_close(unsafe { (*f).fd });
    super::memory::free(f as *mut u8);
    0
}

#[no_mangle]
pub unsafe extern "C" fn fread(buf: *mut u8, size: usize, count: usize, f: *mut FILE) -> usize {
    if f.is_null() || size == 0 || count == 0 {
        return 0;
    }
    let total = size * count;
    let slice = unsafe { core::slice::from_raw_parts_mut(buf, total) };
    let mut read_so_far = 0;
    while read_so_far < total {
        let n = sys_read(unsafe { (*f).fd }, &mut slice[read_so_far..]);
        if n <= 0 {
            if n == 0 { unsafe { (*f).eof = true; } }
            else { unsafe { (*f).error = true; } }
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
    let slice = unsafe { core::slice::from_raw_parts(buf, total) };
    let mut written = 0;
    while written < total {
        let n = sys_write(unsafe { (*f).fd }, &slice[written..]);
        if n <= 0 {
            unsafe { (*f).error = true; }
            break;
        }
        written += n as usize;
    }
    written / size
}

#[no_mangle]
pub unsafe extern "C" fn fseek(f: *mut FILE, offset: i64, whence: i32) -> i32 {
    if f.is_null() { return -1; }
    unsafe { (*f).eof = false; }
    if sys_seek(unsafe { (*f).fd }, offset, whence) >= 0 { 0 } else { -1 }
}

#[no_mangle]
pub unsafe extern "C" fn ftell(f: *mut FILE) -> i64 {
    if f.is_null() { return -1; }
    sys_seek(unsafe { (*f).fd }, 0, 1) // SEEK_CUR
}

#[no_mangle]
pub unsafe extern "C" fn rewind(f: *mut FILE) {
    if !f.is_null() {
        fseek(f, 0, 0);
        unsafe { (*f).error = false; }
    }
}

#[no_mangle]
pub unsafe extern "C" fn feof(f: *mut FILE) -> i32 {
    if f.is_null() { return 0; }
    unsafe { (*f).eof as i32 }
}

#[no_mangle]
pub unsafe extern "C" fn ferror(f: *mut FILE) -> i32 {
    if f.is_null() { return 0; }
    unsafe { (*f).error as i32 }
}

#[no_mangle]
pub unsafe extern "C" fn clearerr(f: *mut FILE) {
    if !f.is_null() {
        unsafe { (*f).eof = false; (*f).error = false; }
    }
}

#[no_mangle]
pub unsafe extern "C" fn fflush(f: *mut FILE) -> i32 {
    if !f.is_null() {
        sys_fsync(unsafe { (*f).fd });
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn fileno(f: *mut FILE) -> i32 {
    if f.is_null() { return -1; }
    unsafe { (*f).fd }
}

#[no_mangle]
pub unsafe extern "C" fn fdopen(fd: i32, _mode: *const u8) -> *mut FILE {
    if fd < 0 { return ptr::null_mut(); }
    let f = super::memory::malloc(core::mem::size_of::<FILE>()) as *mut FILE;
    if f.is_null() { return ptr::null_mut(); }
    unsafe { ptr::write(f, FILE { fd, eof: false, error: false }); }
    f
}

#[no_mangle]
pub unsafe extern "C" fn fgetc(f: *mut FILE) -> i32 {
    let mut c: u8 = 0;
    if fread(&mut c as *mut u8, 1, 1, f) == 1 { c as i32 } else { -1 }
}

#[no_mangle]
pub unsafe extern "C" fn fputc(c: i32, f: *mut FILE) -> i32 {
    let b = c as u8;
    if fwrite(&b as *const u8, 1, 1, f) == 1 { c } else { -1 }
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
        unsafe { *buf.add(i) = c as u8; }
        i += 1;
        if c == b'\n' as i32 { break; }
    }
    unsafe { *buf.add(i) = 0; }
    buf
}

#[no_mangle]
pub unsafe extern "C" fn fputs(s: *const u8, f: *mut FILE) -> i32 {
    let len = super::string::strlen(s);
    fwrite(s, 1, len, f);
    0
}

#[no_mangle]
pub unsafe extern "C" fn getc(f: *mut FILE) -> i32 { fgetc(f) }

#[no_mangle]
pub unsafe extern "C" fn putc(c: i32, f: *mut FILE) -> i32 { fputc(c, f) }

#[no_mangle]
pub unsafe extern "C" fn getchar() -> i32 { fgetc(unsafe { stdin }) }

#[no_mangle]
pub unsafe extern "C" fn putchar(c: i32) -> i32 { fputc(c, unsafe { stdout }) }

#[no_mangle]
pub unsafe extern "C" fn puts(s: *const u8) -> i32 {
    fputs(s, unsafe { stdout });
    fputc(b'\n' as i32, unsafe { stdout });
    0
}

#[no_mangle]
pub unsafe extern "C" fn ungetc(_c: i32, _f: *mut FILE) -> i32 { -1 }

// ---------------------------------------------------------------------------
// File operations
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn remove(path: *const u8) -> i32 {
    sys_delete(unsafe { c_str_bytes(path) })
}

#[no_mangle]
pub unsafe extern "C" fn rename(old: *const u8, new: *const u8) -> i32 {
    sys_rename(unsafe { c_str_bytes(old) }, unsafe { c_str_bytes(new) })
}

#[no_mangle]
pub unsafe extern "C" fn tmpfile() -> *mut FILE { ptr::null_mut() }

#[no_mangle]
pub unsafe extern "C" fn perror(s: *const u8) {
    if !s.is_null() && unsafe { *s } != 0 {
        fputs(s, unsafe { stderr });
        fputs(b": \0".as_ptr(), unsafe { stderr });
    }
    fputs(b"error\n\0".as_ptr(), unsafe { stderr });
}

// ---------------------------------------------------------------------------
// Remaining stdio-adjacent functions
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn system(_command: *const u8) -> i32 { -1 }

#[no_mangle]
pub unsafe extern "C" fn atof(s: *const u8) -> f64 {
    super::misc::strtod(s, ptr::null_mut())
}

#[no_mangle]
pub unsafe extern "C" fn mkdir(path: *const u8, _mode: u32) -> i32 {
    sys_mkdir(unsafe { c_str_bytes(path) })
}

#[no_mangle]
pub unsafe extern "C" fn __assert_fail(expr: *const u8, file: *const u8, _line: i32) {
    fputs(b"assertion failed: \0".as_ptr(), unsafe { stderr });
    fputs(expr, unsafe { stderr });
    fputs(b" at \0".as_ptr(), unsafe { stderr });
    fputs(file, unsafe { stderr });
    fputs(b"\n\0".as_ptr(), unsafe { stderr });
    super::misc::abort();
}

#[no_mangle]
pub static mut errno: i32 = 0;