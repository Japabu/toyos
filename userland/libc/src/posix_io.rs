// POSIX file I/O — thin wrappers around toyos-abi syscalls.

#![allow(non_camel_case_types)]

use core::ptr;

use toyos_abi::Fd;
use toyos_abi::syscall::{self, OpenFlags, SeekFrom};

// ---------------------------------------------------------------------------
// Constants (matching POSIX / Linux values)
// ---------------------------------------------------------------------------

const O_RDONLY: i32 = 0;
const O_WRONLY: i32 = 1;
const O_RDWR: i32 = 2;
const O_CREAT: i32 = 0x40;
const O_TRUNC: i32 = 0x200;
const O_APPEND: i32 = 0x400;

const SEEK_SET: i32 = 0;
const SEEK_CUR: i32 = 1;
const SEEK_END: i32 = 2;

// errno values
const ENOENT: i32 = 2;
const EACCES: i32 = 13;
const EEXIST: i32 = 17;
const EINVAL: i32 = 22;
const EAGAIN: i32 = 11;

// stat file type bits
const S_IFREG: u32 = 0o100000;
const S_IFIFO: u32 = 0o010000;
const S_IFCHR: u32 = 0o020000;

// ---------------------------------------------------------------------------
// Helper: set errno from toyos-abi error
// ---------------------------------------------------------------------------

fn set_errno(e: toyos_abi::syscall::SyscallError) -> i32 {
    use toyos_abi::syscall::SyscallError;
    let code = match e {
        SyscallError::NotFound => ENOENT,
        SyscallError::PermissionDenied => EACCES,
        SyscallError::AlreadyExists => EEXIST,
        SyscallError::InvalidArgument => EINVAL,
        SyscallError::WouldBlock => EAGAIN,
        _ => EINVAL,
    };
    unsafe { super::stdio::errno = code; }
    -1
}

fn fd(raw: i32) -> Fd { Fd(raw) }

pub fn c_str_to_bytes(s: *const u8) -> &'static [u8] {
    unsafe {
        let len = super::string::strlen(s);
        core::slice::from_raw_parts(s, len)
    }
}

// ---------------------------------------------------------------------------
// File descriptor operations
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn open(path: *const u8, flags: i32, _mode: u32) -> i32 {
    let path_bytes = c_str_to_bytes(path);
    let mut oflags = OpenFlags(0);

    let access = flags & 3;
    if access == O_RDONLY || access == O_RDWR { oflags |= OpenFlags::READ; }
    if access == O_WRONLY || access == O_RDWR { oflags |= OpenFlags::WRITE; }
    if flags & O_CREAT != 0 { oflags |= OpenFlags::CREATE; }
    if flags & O_TRUNC != 0 { oflags |= OpenFlags::TRUNCATE; }
    if flags & O_APPEND != 0 { oflags |= OpenFlags::APPEND; }

    match syscall::open(path_bytes, oflags) {
        Ok(f) => f.0,
        Err(e) => set_errno(e),
    }
}

#[no_mangle]
pub unsafe extern "C" fn close(raw_fd: i32) -> i32 {
    syscall::close(fd(raw_fd));
    0
}

#[no_mangle]
pub unsafe extern "C" fn read(raw_fd: i32, buf: *mut u8, count: usize) -> isize {
    if buf.is_null() || count == 0 { return 0; }
    let slice = core::slice::from_raw_parts_mut(buf, count);
    match syscall::read(fd(raw_fd), slice) {
        Ok(n) => n as isize,
        Err(e) => { set_errno(e); -1 }
    }
}

#[no_mangle]
pub unsafe extern "C" fn write(raw_fd: i32, buf: *const u8, count: usize) -> isize {
    if buf.is_null() || count == 0 { return 0; }
    let slice = core::slice::from_raw_parts(buf, count);
    match syscall::write(fd(raw_fd), slice) {
        Ok(n) => n as isize,
        Err(e) => { set_errno(e); -1 }
    }
}

#[no_mangle]
pub unsafe extern "C" fn lseek(raw_fd: i32, offset: i64, whence: i32) -> i64 {
    let pos = match whence {
        SEEK_SET => SeekFrom::Start(offset as u64),
        SEEK_CUR => SeekFrom::Current(offset),
        SEEK_END => SeekFrom::End(offset),
        _ => { super::stdio::errno = EINVAL; return -1; }
    };
    match syscall::seek(fd(raw_fd), pos) {
        Ok(n) => n as i64,
        Err(e) => { set_errno(e); -1 }
    }
}

#[no_mangle]
pub unsafe extern "C" fn fstat(raw_fd: i32, buf: *mut Stat) -> i32 {
    match syscall::fstat(fd(raw_fd)) {
        Ok(st) => {
            if !buf.is_null() {
                ptr::write_bytes(buf, 0, 1);
                let s = &mut *buf;
                s.st_size = st.size as i64;
                s.st_mtime = st.mtime as i64;
                s.st_mode = match st.file_type {
                    syscall::FileType::File => S_IFREG | 0o644,
                    syscall::FileType::Pipe => S_IFIFO | 0o644,
                    syscall::FileType::Tty => S_IFCHR | 0o644,
                    syscall::FileType::Keyboard => S_IFCHR | 0o444,
                    syscall::FileType::Serial => S_IFCHR | 0o644,
                    _ => 0o644,
                };
            }
            0
        }
        Err(e) => set_errno(e),
    }
}

#[no_mangle]
pub unsafe extern "C" fn ftruncate(raw_fd: i32, length: i64) -> i32 {
    match syscall::ftruncate(fd(raw_fd), length as u64) {
        Ok(()) => 0,
        Err(e) => set_errno(e),
    }
}

#[no_mangle]
pub unsafe extern "C" fn fsync(raw_fd: i32) -> i32 {
    match syscall::fsync(fd(raw_fd)) {
        Ok(()) => 0,
        Err(e) => set_errno(e),
    }
}

#[no_mangle]
pub unsafe extern "C" fn dup(raw_fd: i32) -> i32 {
    match syscall::dup(fd(raw_fd)) {
        Ok(f) => f.0,
        Err(e) => set_errno(e),
    }
}

#[no_mangle]
pub unsafe extern "C" fn dup2(old_fd: i32, new_fd: i32) -> i32 {
    if old_fd == new_fd { return new_fd; }
    match syscall::dup2(fd(old_fd), fd(new_fd)) {
        Ok(f) => f.0,
        Err(e) => set_errno(e),
    }
}

#[no_mangle]
pub unsafe extern "C" fn pipe(pipefd: *mut i32) -> i32 {
    let fds = syscall::pipe();
    *pipefd = fds.read.0;
    *pipefd.add(1) = fds.write.0;
    0
}

// ---------------------------------------------------------------------------
// Path operations
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn unlink(path: *const u8) -> i32 {
    match syscall::delete(c_str_to_bytes(path)) {
        Ok(()) => 0,
        Err(e) => set_errno(e),
    }
}

#[no_mangle]
pub unsafe extern "C" fn rmdir(path: *const u8) -> i32 {
    match syscall::rmdir(c_str_to_bytes(path)) {
        Ok(()) => 0,
        Err(e) => set_errno(e),
    }
}

#[no_mangle]
pub unsafe extern "C" fn getcwd(buf: *mut u8, size: usize) -> *mut u8 {
    if buf.is_null() || size == 0 { return ptr::null_mut(); }
    let slice = core::slice::from_raw_parts_mut(buf, size);
    let n = syscall::getcwd(slice);
    if n == 0 || n >= size {
        return ptr::null_mut();
    }
    *buf.add(n) = 0; // null-terminate
    buf
}

#[no_mangle]
pub unsafe extern "C" fn chdir(path: *const u8) -> i32 {
    match syscall::chdir(c_str_to_bytes(path)) {
        Ok(()) => 0,
        Err(e) => set_errno(e),
    }
}

// stat by path: open + fstat + close
#[no_mangle]
pub unsafe extern "C" fn stat(path: *const u8, buf: *mut Stat) -> i32 {
    stat_impl(path, buf)
}

#[no_mangle]
pub unsafe extern "C" fn lstat(path: *const u8, buf: *mut Stat) -> i32 {
    // ToyOS has no symlinks, lstat = stat
    stat_impl(path, buf)
}

unsafe fn stat_impl(path: *const u8, buf: *mut Stat) -> i32 {
    let path_bytes = c_str_to_bytes(path);
    // Try opening read-only
    match syscall::open(path_bytes, OpenFlags::READ) {
        Ok(f) => {
            let result = fstat(f.0, buf);
            syscall::close(f);
            result
        }
        Err(e) => set_errno(e),
    }
}

#[no_mangle]
pub unsafe extern "C" fn access(path: *const u8, _mode: i32) -> i32 {
    // ToyOS has no permissions model; just check existence
    let path_bytes = c_str_to_bytes(path);
    match syscall::open(path_bytes, OpenFlags::READ) {
        Ok(f) => { syscall::close(f); 0 }
        Err(e) => set_errno(e),
    }
}

#[no_mangle]
pub unsafe extern "C" fn isatty(raw_fd: i32) -> i32 {
    match syscall::fstat(fd(raw_fd)) {
        Ok(st) => (st.file_type == syscall::FileType::Tty || st.file_type == syscall::FileType::Keyboard) as i32,
        Err(_) => 0,
    }
}

#[no_mangle]
pub unsafe extern "C" fn chmod(_path: *const u8, _mode: u32) -> i32 { 0 }

#[no_mangle]
pub unsafe extern "C" fn fchmod(_fd: i32, _mode: u32) -> i32 { 0 }

static mut UMASK_VAL: u32 = 0o022;

#[no_mangle]
pub unsafe extern "C" fn umask(mask: u32) -> u32 {
    let old = unsafe { UMASK_VAL };
    unsafe { UMASK_VAL = mask & 0o777; }
    old
}

#[no_mangle]
pub unsafe extern "C" fn fcntl(_fd: i32, _cmd: i32, _arg: i64) -> i32 {
    // Stub — return success for common operations
    0
}

// pread/pwrite: emulate with seek + read/write + seek back
#[no_mangle]
pub unsafe extern "C" fn pread(raw_fd: i32, buf: *mut u8, count: usize, offset: i64) -> isize {
    let old = lseek(raw_fd, 0, SEEK_CUR);
    if old < 0 { return -1; }
    if lseek(raw_fd, offset, SEEK_SET) < 0 { return -1; }
    let n = read(raw_fd, buf, count);
    lseek(raw_fd, old, SEEK_SET);
    n
}

#[no_mangle]
pub unsafe extern "C" fn pwrite(raw_fd: i32, buf: *const u8, count: usize, offset: i64) -> isize {
    let old = lseek(raw_fd, 0, SEEK_CUR);
    if old < 0 { return -1; }
    if lseek(raw_fd, offset, SEEK_SET) < 0 { return -1; }
    let n = write(raw_fd, buf, count);
    lseek(raw_fd, old, SEEK_SET);
    n
}

// ---------------------------------------------------------------------------
// Directory operations
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct DIR {
    buf: *mut u8,
    len: usize,
    pos: usize,
}

#[repr(C)]
pub struct dirent {
    pub d_ino: u64,
    pub d_type: u8,
    pub d_name: [u8; 256],
}

const DT_REG: u8 = 8;

#[no_mangle]
pub unsafe extern "C" fn opendir(path: *const u8) -> *mut DIR {
    let path_bytes = c_str_to_bytes(path);
    let buf_size = 65536;
    let buf = super::memory::malloc(buf_size);
    if buf.is_null() { return ptr::null_mut(); }

    let slice = core::slice::from_raw_parts_mut(buf, buf_size);
    let n = syscall::readdir(path_bytes, slice);

    let dir = super::memory::malloc(core::mem::size_of::<DIR>()) as *mut DIR;
    if dir.is_null() {
        super::memory::free(buf);
        return ptr::null_mut();
    }
    ptr::write(dir, DIR { buf, len: n, pos: 0 });
    dir
}

#[no_mangle]
pub unsafe extern "C" fn readdir(dir: *mut DIR) -> *mut dirent {
    if dir.is_null() { return ptr::null_mut(); }
    let d = &mut *dir;
    if d.pos >= d.len { return ptr::null_mut(); }

    // Entries are null-separated in the buffer
    let start = d.pos;
    while d.pos < d.len && *d.buf.add(d.pos) != 0 {
        d.pos += 1;
    }
    let name_len = d.pos - start;
    if d.pos < d.len { d.pos += 1; } // skip null

    // Use a static buffer for the dirent (not thread-safe, matching POSIX convention)
    static mut DIRENT_BUF: dirent = dirent { d_ino: 0, d_type: 0, d_name: [0; 256] };
    let ent = &raw mut DIRENT_BUF;
    (*ent).d_ino = (start + 1) as u64;
    (*ent).d_type = DT_REG; // We don't have type info in readdir buffer, default to file
    let copy_len = name_len.min(255);
    ptr::copy_nonoverlapping(d.buf.add(start), (*ent).d_name.as_mut_ptr(), copy_len);
    (*ent).d_name[copy_len] = 0;
    ent
}

#[no_mangle]
pub unsafe extern "C" fn closedir(dir: *mut DIR) -> i32 {
    if dir.is_null() { return -1; }
    let d = &*dir;
    super::memory::free(d.buf);
    super::memory::free(dir as *mut u8);
    0
}

// ---------------------------------------------------------------------------
// struct stat
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct Stat {
    pub st_dev: u64,
    pub st_ino: u64,
    pub st_mode: u32,
    pub st_nlink: u32,
    pub st_uid: u32,
    pub st_gid: u32,
    pub st_rdev: u64,
    pub st_size: i64,
    pub st_blksize: i64,
    pub st_blocks: i64,
    pub st_atime: i64,
    pub st_mtime: i64,
    pub st_ctime: i64,
}

// ---------------------------------------------------------------------------
// mmap/munmap (real implementations using toyos-abi)
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn mmap(
    addr: *mut u8, len: usize, prot: i32, flags: i32, _fd: i32, _offset: i64,
) -> *mut u8 {
    use toyos_abi::syscall::{MmapProt, MmapFlags};

    let mut mp = MmapProt::NONE;
    if prot & 1 != 0 { mp = mp | MmapProt::READ; }
    if prot & 2 != 0 { mp = mp | MmapProt::WRITE; }

    let mut mf = MmapFlags::PRIVATE;
    if flags & 0x20 != 0 { mf = mf | MmapFlags::ANONYMOUS; }
    if flags & 0x10 != 0 { mf = mf | MmapFlags::FIXED; }

    let ptr = unsafe { syscall::mmap(addr, len, mp, mf) };
    if ptr.is_null() {
        usize::MAX as *mut u8 // MAP_FAILED
    } else {
        ptr
    }
}

#[no_mangle]
pub unsafe extern "C" fn munmap(addr: *mut u8, len: usize) -> i32 {
    // SAFETY: caller is responsible for addr/len matching a previous mmap
    match unsafe { syscall::munmap(addr, len) } {
        Ok(()) => 0,
        Err(e) => set_errno(e),
    }
}

// ---------------------------------------------------------------------------
// poll
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct pollfd {
    pub fd: i32,
    pub events: i16,
    pub revents: i16,
}

const POLLIN: i16 = 1;
const POLLOUT: i16 = 4;

#[no_mangle]
pub unsafe extern "C" fn poll(fds: *mut pollfd, nfds: u32, timeout: i32) -> i32 {
    if nfds == 0 {
        if timeout > 0 {
            syscall::nanosleep(timeout as u64 * 1_000_000);
        }
        return 0;
    }

    let timeout_ns = if timeout < 0 { None } else { Some(timeout as u64 * 1_000_000) };

    let n = nfds as usize;
    let poller = toyos::poller::Poller::new(n as u32);
    for i in 0..n {
        let pfd = &*fds.add(i);
        let mut flags = 0u32;
        if pfd.events & POLLIN != 0 { flags |= toyos::poller::IORING_POLL_IN; }
        if pfd.events & POLLOUT != 0 { flags |= toyos::poller::IORING_POLL_OUT; }
        poller.poll_add_fd(toyos_abi::Fd(pfd.fd), flags, i as u64);
    }

    let mut ready_set = alloc::vec![false; n];
    poller.wait(1, timeout_ns.unwrap_or(u64::MAX), |token| {
        if (token as usize) < n { ready_set[token as usize] = true; }
    });
    let mut ready = 0i32;
    for i in 0..n {
        let pfd = &mut *fds.add(i);
        pfd.revents = 0;
        if ready_set[i] {
            pfd.revents = pfd.events;
            ready += 1;
        }
    }
    ready
}