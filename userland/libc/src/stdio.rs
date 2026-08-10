use core::ptr;
use toyos_abi::RawHandle;
use toyos_abi::syscall::{self, OpenFlags, SeekFrom};

// Platform fd operations

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

fn sys_close(fd: i32) { syscall::close(RawHandle(fd as u32)); }

fn sys_read(fd: i32, buf: &mut [u8]) -> isize {
    match syscall::read(RawHandle(fd as u32), buf) {
        Ok(n) => n as isize,
        Err(_) => -1,
    }
}

fn sys_write(fd: i32, buf: &[u8]) -> isize {
    match syscall::write(RawHandle(fd as u32), buf) {
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
    match syscall::seek(RawHandle(fd as u32), pos) {
        Ok(n) => n as i64,
        Err(_) => -1,
    }
}

fn sys_fsync(fd: i32) { let _ = syscall::fsync(RawHandle(fd as u32)); }

fn sys_delete(path: &[u8]) -> i32 {
    match syscall::delete(path) { Ok(()) => 0, Err(_) => -1 }
}

fn sys_rename(old: &[u8], new: &[u8]) -> i32 {
    match syscall::rename(old, new) { Ok(()) => 0, Err(_) => -1 }
}

fn sys_mkdir(path: &[u8]) -> i32 {
    match syscall::mkdir(path) { Ok(()) => 0, Err(_) => -1 }
}

// FILE struct

/// Matches `BUFSIZ` in `include/stdio.h`. C programs size their own buffers
/// against that macro, so the two must not drift.
const BUFSIZ: usize = 8192;

// Buffering modes, by their C values (`_IONBF` / `_IOLBF` / `_IOFBF`).
const IOFBF: i32 = 0;
const IOLBF: i32 = 1;
const IONBF: i32 = 2;

/// Buffering not yet decided. Resolved on first write, because deciding needs
/// an `fstat` and a program that never prints should not pay for one.
const MODE_UNSET: i32 = -1;

/// A C stream.
///
/// The buffer is not a performance feature, it is a correctness one. Without it
/// every `fputc` was its own `write` syscall and `puts` was two — `fputs` then
/// the newline — so a kernel `log!` landing between them cut the line in half
/// and took its newline with it. The kernel side is already writer-atomic: one
/// `write` commits one chunk under the ring lock, so a line that reaches the
/// kernel in a single call cannot be split. Buffering is what makes a line one
/// call. C requires it anyway; this was a conformance gap, not a workaround.
///
/// Not thread-safe: there is no `flockfile`, matching the rest of this libc
/// (`errno` and the `atexit` table are plain statics too). Concurrent writes to
/// one stream can interleave inside a line.
pub struct FILE {
    fd: i32,
    eof: bool,
    error: bool,
    /// `IOFBF` / `IOLBF` / `IONBF`, or `MODE_UNSET` until the first write.
    mode: i32,
    /// Pending output. Null means this stream is unbuffered no matter what
    /// `mode` says.
    buf: *mut u8,
    cap: usize,
    len: usize,
    /// Whether `buf` came from `malloc` and so must be freed by `fclose`.
    owned: bool,
    /// Next stream in the open list, for `fflush(NULL)`.
    next: *mut FILE,
}

const STDIN_FD: i32 = 0;
const STDOUT_FD: i32 = 1;
const STDERR_FD: i32 = 2;

static mut STDOUT_BUF: [u8; BUFSIZ] = [0; BUFSIZ];

/// `stdout` carries the only static buffer. `stderr` is unbuffered by C's rule
/// that it must not be fully buffered — output has to survive a crash that
/// never reaches a flush, which is why `abort` can skip flushing at all. We
/// buffer no reads, so `stdin` needs none either.
static mut STDOUT_FILE: FILE = FILE {
    fd: STDOUT_FD, eof: false, error: false,
    mode: MODE_UNSET, buf: (&raw mut STDOUT_BUF) as *mut u8, cap: BUFSIZ, len: 0,
    owned: false, next: &raw mut STDERR_FILE,
};
static mut STDERR_FILE: FILE = FILE {
    fd: STDERR_FD, eof: false, error: false,
    mode: IONBF, buf: ptr::null_mut(), cap: 0, len: 0,
    owned: false, next: ptr::null_mut(),
};
static mut STDIN_FILE: FILE = FILE {
    fd: STDIN_FD, eof: false, error: false,
    mode: IONBF, buf: ptr::null_mut(), cap: 0, len: 0,
    owned: false, next: ptr::null_mut(),
};

/// Head of the open-stream list. `fflush(NULL)` walks it, which is what makes
/// `exit`'s flush reach an `fopen`ed file rather than only the std streams.
static mut OPEN_STREAMS: *mut FILE = &raw mut STDOUT_FILE;

unsafe fn register_stream(f: *mut FILE) {
    unsafe {
        (*f).next = ptr::addr_of!(OPEN_STREAMS).read();
        ptr::addr_of_mut!(OPEN_STREAMS).write(f);
    }
}

unsafe fn unregister_stream(f: *mut FILE) {
    unsafe {
        let head = ptr::addr_of!(OPEN_STREAMS).read();
        if head == f {
            ptr::addr_of_mut!(OPEN_STREAMS).write((*f).next);
            return;
        }
        let mut cur = head;
        while !cur.is_null() {
            if (*cur).next == f {
                (*cur).next = (*f).next;
                return;
            }
            cur = (*cur).next;
        }
    }
}

/// Whether this fd names a device a human is watching.
///
/// Deliberately not `isatty`, which answers the narrower POSIX question and
/// returns false for `FileType::Serial` — which is exactly what every ToyOS
/// process gets for fds 0, 1 and 2, since `spawn_kernel` hands out
/// `Descriptor::SerialConsole`. Routing that through `isatty` would make every
/// program's stdout *fully* buffered: output would appear only at exit, and a
/// program that wedged or was killed on a timeout would lose all of it — the
/// failure mode that is hardest to tell apart from the program never having run.
/// C's rule is "interactive device", not "terminal", and a serial console is one.
unsafe fn fd_is_interactive(fd: i32) -> bool {
    match syscall::fstat(RawHandle(fd as u32)) {
        Ok(st) => matches!(
            st.file_type,
            syscall::FileType::Tty | syscall::FileType::Serial | syscall::FileType::Keyboard
        ),
        Err(_) => false,
    }
}

/// Decide this stream's buffering, once, on first write.
///
/// Line-buffered on an interactive device and fully buffered otherwise is C's
/// rule, and the one that makes a redirected program cheap and a console one
/// readable. A stream with no buffer is unbuffered whatever else is true.
unsafe fn ensure_mode(f: *mut FILE) {
    unsafe {
        if (*f).mode != MODE_UNSET {
            return;
        }
        (*f).mode = if (*f).buf.is_null() {
            IONBF
        } else if fd_is_interactive((*f).fd) {
            IOLBF
        } else {
            IOFBF
        };
    }
}

/// Write `data` to the fd, looping over short writes. Sets the error flag and
/// stops on failure, like the unbuffered path always did.
unsafe fn write_all(f: *mut FILE, data: &[u8]) -> usize {
    let mut written = 0;
    while written < data.len() {
        let n = sys_write(unsafe { (*f).fd }, &data[written..]);
        if n <= 0 {
            unsafe { (*f).error = true; }
            break;
        }
        written += n as usize;
    }
    written
}

/// Push the whole pending buffer out.
///
/// `len` is cleared before the write, not after: a stream whose fd has died
/// would otherwise keep the same bytes pending and retry them on every
/// subsequent call, turning one failed write into an unbounded number.
unsafe fn flush_buf(f: *mut FILE) -> i32 {
    unsafe {
        let len = (*f).len;
        if len == 0 || (*f).buf.is_null() {
            return 0;
        }
        (*f).len = 0;
        let data = core::slice::from_raw_parts((*f).buf, len);
        if write_all(f, data) == len { 0 } else { -1 }
    }
}

/// Flush only up to and including the last newline, keeping any partial line.
///
/// This is the whole point of line buffering: `puts` puts "text" in the buffer
/// and then a newline, and this hands the kernel "text\n" in one `write`.
/// Flushing the entire buffer instead would emit a partial next line and
/// re-create the splice this exists to remove.
unsafe fn flush_lines(f: *mut FILE) -> i32 {
    unsafe {
        let len = (*f).len;
        if len == 0 || (*f).buf.is_null() {
            return 0;
        }
        let data = core::slice::from_raw_parts((*f).buf, len);
        let Some(pos) = data.iter().rposition(|&b| b == b'\n') else {
            return 0;
        };
        let through = pos + 1;
        let rest = len - through;
        // Move the remainder down before writing, for the same reason
        // `flush_buf` clears `len` first: a failed write must not leave bytes
        // staged for a retry that would duplicate them.
        (*f).len = rest;
        let staged = core::slice::from_raw_parts((*f).buf, through);
        let ok = write_all(f, staged) == through;
        if rest > 0 {
            ptr::copy((*f).buf.add(through), (*f).buf, rest);
        }
        if ok { 0 } else { -1 }
    }
}

/// The one path every stdio write funnels through.
///
/// Returns bytes accepted, which for a buffered stream counts bytes that are in
/// the buffer rather than on the fd — the same thing C reports.
unsafe fn write_bytes(f: *mut FILE, data: &[u8]) -> usize {
    unsafe {
        ensure_mode(f);
        if (*f).mode == IONBF || (*f).buf.is_null() {
            return write_all(f, data);
        }

        let mut done = 0;
        while done < data.len() {
            let room = (*f).cap - (*f).len;
            if room == 0 {
                if flush_buf(f) != 0 {
                    return done;
                }
                continue;
            }
            let take = room.min(data.len() - done);
            ptr::copy_nonoverlapping(data.as_ptr().add(done), (*f).buf.add((*f).len), take);
            (*f).len += take;
            done += take;
        }

        if (*f).mode == IOLBF && data.contains(&b'\n') && flush_lines(f) != 0 {
            return done;
        }
        done
    }
}

/// Push out anything pending before the fd's offset is read or moved.
///
/// Buffered writes have not reached the fd yet, so a seek or a read taken
/// against a stream with a pending buffer would use the wrong offset and land
/// the bytes in the wrong place.
unsafe fn sync_before_fd_use(f: *mut FILE) {
    unsafe {
        if (*f).len > 0 {
            flush_buf(f);
        }
    }
}

#[no_mangle]
pub static mut stdout: *mut FILE = &raw mut STDOUT_FILE;
#[no_mangle]
pub static mut stderr: *mut FILE = &raw mut STDERR_FILE;
#[no_mangle]
pub static mut stdin: *mut FILE = &raw mut STDIN_FILE;

// FILE I/O

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
    unsafe { ptr::write(f, new_stream(fd)); register_stream(f); }
    f
}

/// A stream with a buffer if one can be had, and an unbuffered one if not.
///
/// A failed `malloc` degrades to unbuffered rather than failing the open: the
/// stream still works and still writes every byte, it just costs a syscall per
/// call. Losing the buffer is a performance loss; losing the open would be a
/// behaviour change.
unsafe fn new_stream(fd: i32) -> FILE {
    let buf = super::memory::malloc(BUFSIZ);
    FILE {
        fd, eof: false, error: false,
        mode: MODE_UNSET,
        buf,
        cap: if buf.is_null() { 0 } else { BUFSIZ },
        len: 0,
        owned: !buf.is_null(),
        next: ptr::null_mut(),
    }
}

#[no_mangle]
pub unsafe extern "C" fn fclose(f: *mut FILE) -> i32 {
    if f.is_null() || f == unsafe { stdout } || f == unsafe { stderr } || f == unsafe { stdin } {
        return -1;
    }
    unsafe {
        // Before the close, or the pending bytes go nowhere and a buffer turns
        // a visible-output bug into a lost-output one.
        let flushed = flush_buf(f);
        unregister_stream(f);
        sys_close((*f).fd);
        if (*f).owned && !(*f).buf.is_null() {
            super::memory::free((*f).buf);
        }
        super::memory::free(f as *mut u8);
        flushed
    }
}

#[no_mangle]
pub unsafe extern "C" fn fread(buf: *mut u8, size: usize, count: usize, f: *mut FILE) -> usize {
    if f.is_null() || size == 0 || count == 0 {
        return 0;
    }
    let Some(total) = size.checked_mul(count) else { return 0 };
    unsafe { sync_before_fd_use(f); }
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
    let Some(total) = size.checked_mul(count) else { return 0 };
    let slice = unsafe { core::slice::from_raw_parts(buf, total) };
    unsafe { write_bytes(f, slice) / size }
}

#[no_mangle]
pub unsafe extern "C" fn fseek(f: *mut FILE, offset: i64, whence: i32) -> i32 {
    if f.is_null() { return -1; }
    unsafe { sync_before_fd_use(f); (*f).eof = false; }
    if sys_seek(unsafe { (*f).fd }, offset, whence) >= 0 { 0 } else { -1 }
}

#[no_mangle]
pub unsafe extern "C" fn ftell(f: *mut FILE) -> i64 {
    if f.is_null() { return -1; }
    // Pending bytes have not moved the fd's offset yet, so reporting it now
    // would be short by exactly what is still in the buffer.
    unsafe { sync_before_fd_use(f); }
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

/// `fflush(NULL)` flushes every output stream, which is what C says and what
/// `exit` has always called it for — until now that call did nothing, because
/// there was no buffer to flush and the null case returned early.
///
/// The `fsync` is kept from the previous behaviour, and stays in the public
/// entry point rather than in `flush_buf`: internal flushing now happens once
/// per line, and pushing the filesystem that hard per line would be a large
/// unasked-for slowdown.
#[no_mangle]
pub unsafe extern "C" fn fflush(f: *mut FILE) -> i32 {
    unsafe {
        if f.is_null() {
            let mut result = 0;
            let mut cur = ptr::addr_of!(OPEN_STREAMS).read();
            while !cur.is_null() {
                if flush_buf(cur) != 0 {
                    result = -1;
                }
                cur = (*cur).next;
            }
            return result;
        }
        let result = flush_buf(f);
        sys_fsync((*f).fd);
        result
    }
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
    unsafe { ptr::write(f, new_stream(fd)); register_stream(f); }
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

// File operations

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

// Remaining stdio-adjacent functions

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