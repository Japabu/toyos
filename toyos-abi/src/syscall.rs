// Syscall numbers (must match kernel dispatch table)
pub const SYS_WRITE: u64 = 0;
pub const SYS_READ: u64 = 1;
// Syscall numbers 2-4 are unused (formerly SYS_ALLOC/FREE/REALLOC).
pub const SYS_THREAD_EXIT: u64 = 5;
pub const SYS_RANDOM: u64 = 6;
pub const SYS_SCREEN_SIZE: u64 = 7;
pub const SYS_CLOCK: u64 = 8;
pub const SYS_OPEN: u64 = 9;
pub const SYS_CLOSE: u64 = 10;
pub const SYS_SEEK: u64 = 13;
pub const SYS_FSTAT: u64 = 14;
pub const SYS_FSYNC: u64 = 15;
pub const SYS_READDIR: u64 = 17;
pub const SYS_DELETE: u64 = 18;
pub const SYS_SHUTDOWN: u64 = 19;
pub const SYS_CHDIR: u64 = 20;
pub const SYS_GETCWD: u64 = 21;
pub const SYS_SET_KEYBOARD_LAYOUT: u64 = 23;
pub const SYS_PIPE: u64 = 24;
pub const SYS_SPAWN: u64 = 25;
pub const SYS_WAITPID: u64 = 26;
pub const SYS_MARK_TTY: u64 = 28;
// Syscall numbers 29-30 unused (formerly SYS_SEND_MSG/SYS_RECV_MSG).
pub const SYS_OPEN_DEVICE: u64 = 31;
// Syscall numbers 32-33 unused (formerly SYS_REGISTER_NAME/SYS_FIND_PID).
pub const SYS_SET_SCREEN_SIZE: u64 = 34;
pub const SYS_GPU_PRESENT: u64 = 35;
pub const SYS_ALLOC_SHARED: u64 = 36;
pub const SYS_GRANT_SHARED: u64 = 37;
pub const SYS_MAP_SHARED: u64 = 38;
pub const SYS_RELEASE_SHARED: u64 = 39;
pub const SYS_THREAD_SPAWN: u64 = 40;
pub const SYS_THREAD_JOIN: u64 = 41;
pub const SYS_CLOCK_REALTIME: u64 = 42;
pub const SYS_GPU_SET_CURSOR: u64 = 43;
pub const SYS_GPU_MOVE_CURSOR: u64 = 44;
pub const SYS_SYSINFO: u64 = 45;
pub const SYS_NET_INFO: u64 = 46;
pub const SYS_NET_SEND: u64 = 47;
pub const SYS_NET_RECV: u64 = 48;
pub const SYS_NANOSLEEP: u64 = 49;
pub const SYS_DUP: u64 = 50;
pub const SYS_GETPID: u64 = 51;
pub const SYS_RENAME: u64 = 52;
pub const SYS_MKDIR: u64 = 53;
pub const SYS_RMDIR: u64 = 54;
pub const SYS_DLOPEN: u64 = 55;
pub const SYS_DLSYM: u64 = 56;
pub const SYS_DLCLOSE: u64 = 57;
pub const SYS_FUTEX_WAIT: u64 = 58;
pub const SYS_FUTEX_WAKE: u64 = 59;
pub const SYS_FTRUNCATE: u64 = 60;
pub const SYS_STACK_INFO: u64 = 61;
pub const SYS_CPU_COUNT: u64 = 62;
pub const SYS_MMAP: u64 = 63;
pub const SYS_MUNMAP: u64 = 64;
pub const SYS_KILL: u64 = 65;
pub const SYS_READ_NONBLOCK: u64 = 66;
pub const SYS_WRITE_NONBLOCK: u64 = 67;
pub const SYS_PIPE_OPEN: u64 = 68;
pub const SYS_PIPE_ID: u64 = 70;
pub const SYS_AUDIO_SUBMIT: u64 = 71;
pub const SYS_AUDIO_POLL: u64 = 84;
pub const SYS_EXIT: u64 = 72;
pub const SYS_GET_ENV: u64 = 73;
pub const SYS_DUP2: u64 = 74;
pub const SYS_CLOCK_EPOCH: u64 = 75;
pub const SYS_SOCKET_CREATE: u64 = 76;
pub const SYS_PIPE_MAP: u64 = 77;
pub const SYS_NIC_RX_POLL: u64 = 78;
pub const SYS_NIC_RX_DONE: u64 = 79;
pub const SYS_NIC_TX: u64 = 80;
pub const SYS_SYMLINK: u64 = 81;
pub const SYS_READLINK: u64 = 82;
pub const SYS_GPU_SET_RESOLUTION: u64 = 83;
pub const SYS_LISTEN: u64 = 85;
pub const SYS_ACCEPT: u64 = 86;
pub const SYS_CONNECT: u64 = 87;
/// Allocate a TLS block for a dlopen'd module on the current thread.
/// Arg0: module_id (1-based DTV index). Returns physical address of allocated block.
pub const SYS_TLS_ALLOC_BLOCK: u64 = 88;
pub const SYS_IO_URING_SETUP: u64 = 89;
pub const SYS_IO_URING_ENTER: u64 = 90;
pub const SYS_QUERY_MODULES: u64 = 91;
/// Debug syscall. Arg0 selects the action:
///   0 = kernel panic (triggers panic!() in syscall context)
///   1 = kernel fault (null pointer deref in kernel context)
pub const SYS_DEBUG: u64 = 92;
pub const SYS_SCHED_INFO: u64 = 93;
pub const SYS_PROCESS_STATS: u64 = 94;
pub const SYS_SET_THREAD_NAME: u64 = 95;

pub const WNOHANG: u64 = 1;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Arguments for the `SYS_SPAWN` syscall, passed as a single pointer.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SpawnArgs {
    pub argv_ptr: u64,
    pub argv_len: u64,
    pub fd_map_ptr: u64,
    pub fd_map_count: u64,
    pub env_ptr: u64,
    pub env_len: u64,
}

use crate::{Fd, Pid};

/// Syscall error with a specific code. Values occupy the top of the u64 range:
/// error code N is encoded as `u64::MAX - N`. Any return value `>= u64::MAX - 255`
/// is an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
pub enum SyscallError {
    Unknown = 0,
    NotFound = 1,
    PermissionDenied = 2,
    AlreadyExists = 3,
    InvalidArgument = 4,
    BadAddress = 5,
    WouldBlock = 6,
    ResourceExhausted = 7,
    NotSupported = 8,
}

impl SyscallError {
    pub const fn to_u64(self) -> u64 {
        u64::MAX - self as u64
    }

    pub fn from_u64(val: u64) -> Option<Self> {
        if val < u64::MAX - 255 {
            return None;
        }
        let code = u64::MAX - val;
        match code {
            0 => Some(Self::Unknown),
            1 => Some(Self::NotFound),
            2 => Some(Self::PermissionDenied),
            3 => Some(Self::AlreadyExists),
            4 => Some(Self::InvalidArgument),
            5 => Some(Self::BadAddress),
            6 => Some(Self::WouldBlock),
            7 => Some(Self::ResourceExhausted),
            8 => Some(Self::NotSupported),
            _ => Some(Self::Unknown),
        }
    }
}

impl core::fmt::Display for SyscallError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Unknown => f.write_str("unknown error"),
            Self::NotFound => f.write_str("not found"),
            Self::PermissionDenied => f.write_str("permission denied"),
            Self::AlreadyExists => f.write_str("already exists"),
            Self::InvalidArgument => f.write_str("invalid argument"),
            Self::BadAddress => f.write_str("bad address"),
            Self::WouldBlock => f.write_str("would block"),
            Self::ResourceExhausted => f.write_str("resource exhausted"),
            Self::NotSupported => f.write_str("not supported"),
        }
    }
}

/// Check a raw syscall return value: if it's an error, return Err; otherwise Ok(val).
fn check(val: u64) -> Result<u64, SyscallError> {
    match SyscallError::from_u64(val) {
        Some(e) => Err(e),
        None => Ok(val),
    }
}

/// Check a raw syscall return for success (0) or error.
fn check_unit(val: u64) -> Result<(), SyscallError> {
    check(val).map(|_| ())
}

/// File type for file descriptors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u64)]
pub enum FileType {
    #[default]
    Unknown = 0,
    File = 1,
    Pipe = 2,
    Keyboard = 3,
    Serial = 4,
    Framebuffer = 5,
    Tty = 6,
    Mouse = 7,
    Socket = 8,
    Nic = 9,
}

impl FileType {
    pub fn from_u64(val: u64) -> Option<Self> {
        match val {
            0 => Some(Self::Unknown),
            1 => Some(Self::File),
            2 => Some(Self::Pipe),
            3 => Some(Self::Keyboard),
            4 => Some(Self::Serial),
            5 => Some(Self::Framebuffer),
            6 => Some(Self::Tty),
            7 => Some(Self::Mouse),
            8 => Some(Self::Socket),
            9 => Some(Self::Nic),
            _ => None,
        }
    }
}

/// Seek position for [`seek`].
pub enum SeekFrom {
    Start(u64),
    Current(i64),
    End(i64),
}

/// Flags for [`open`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenFlags(pub u64);

impl OpenFlags {
    pub const READ: Self = Self(1);
    pub const WRITE: Self = Self(2);
    pub const CREATE: Self = Self(4);
    pub const TRUNCATE: Self = Self(8);
    pub const APPEND: Self = Self(16);

    pub const fn contains(self, flag: Self) -> bool { self.0 & flag.0 != 0 }
}

impl core::ops::BitOr for OpenFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self { Self(self.0 | rhs.0) }
}

impl core::ops::BitOrAssign for OpenFlags {
    fn bitor_assign(&mut self, rhs: Self) { self.0 |= rhs.0; }
}

/// Memory protection flags for [`mmap`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MmapProt(pub u64);

impl MmapProt {
    pub const NONE: Self = Self(0);
    pub const READ: Self = Self(1);
    pub const WRITE: Self = Self(2);
}

impl core::ops::BitOr for MmapProt {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self { Self(self.0 | rhs.0) }
}

/// Memory mapping flags for [`mmap`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MmapFlags(pub u64);

impl MmapFlags {
    pub const ANONYMOUS: Self = Self(1);
    pub const PRIVATE: Self = Self(2);
    pub const FIXED: Self = Self(4);
}

impl core::ops::BitOr for MmapFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self { Self(self.0 | rhs.0) }
}

/// Result of [`pipe`]: the read and write ends.
#[derive(Debug, Clone, Copy)]
pub struct PipeFds {
    pub read: Fd,
    pub write: Fd,
}

/// Wall-clock time from RTC.
#[derive(Debug, Clone, Copy)]
pub struct RealTime {
    pub hours: u8,
    pub minutes: u8,
    pub seconds: u8,
}

/// File metadata returned by [`fstat`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Stat {
    pub file_type: FileType,
    pub size: u64,
    /// Last modification time (nanoseconds since boot).
    pub mtime: u64,
}

// ---------------------------------------------------------------------------
// Raw syscall
// ---------------------------------------------------------------------------

#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn syscall(num: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            in("rdi") num,
            in("rsi") a1,
            in("rdx") a2,
            in("r8") a3,
            in("r9") a4,
            lateout("rax") ret,
            out("rcx") _,
            out("r11") _,
        );
    }
    ret
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
fn syscall(num: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "svc #0",
            in("x0") num,
            in("x1") a1,
            in("x2") a2,
            in("x3") a3,
            in("x4") a4,
            lateout("x0") ret,
        );
    }
    ret
}

/// Encode an optional timeout for the kernel ABI.
/// `None` = wait forever (u64::MAX), `Some(n)` = timeout after `n` nanoseconds.
fn encode_timeout(timeout: Option<u64>) -> u64 {
    match timeout {
        None => u64::MAX,
        Some(n) => n,
    }
}

// ---------------------------------------------------------------------------
// Wrappers
// ---------------------------------------------------------------------------

// --- I/O ---

/// Write bytes to a file descriptor. Returns number of bytes written.
pub fn write(fd: Fd, buf: &[u8]) -> Result<usize, SyscallError> {
    check(syscall(SYS_WRITE, fd.0 as u64, buf.as_ptr() as u64, buf.len() as u64, 0)).map(|n| n as usize)
}

/// Read bytes from a file descriptor. Returns number of bytes read.
pub fn read(fd: Fd, buf: &mut [u8]) -> Result<usize, SyscallError> {
    check(syscall(SYS_READ, fd.0 as u64, buf.as_mut_ptr() as u64, buf.len() as u64, 0)).map(|n| n as usize)
}

// --- Process ---

/// Exit the current thread only. Does not return.
/// Use `exit()` to exit the entire process (all threads).
pub fn thread_exit(code: i32) -> ! {
    loop { syscall(SYS_THREAD_EXIT, code as u64, 0, 0, 0); }
}

/// Exit the entire process (all threads) with `code`. Does not return.
pub fn exit(code: i32) -> ! {
    loop { syscall(SYS_EXIT, code as u64, 0, 0, 0); }
}

/// Debug syscall. `action`: 0 = kernel panic, 1 = kernel fault.
pub fn debug(action: u64) -> u64 {
    syscall(SYS_DEBUG, action, 0, 0, 0)
}

/// Create a pipe. Returns the read and write file descriptors.
pub fn pipe() -> PipeFds {
    let raw = syscall(SYS_PIPE, 0, 0, 0, 0);
    PipeFds {
        read: Fd((raw >> 32) as i32),
        write: Fd((raw & 0xFFFF_FFFF) as i32),
    }
}

/// Read the inherited environment variables into `buf`.
/// Returns the number of bytes written, or the required size if buf is too small.
pub fn get_env(buf: &mut [u8]) -> usize {
    syscall(SYS_GET_ENV, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0) as usize
}

/// Spawn a new process. The `SpawnArgs` struct contains argv, fd_map, and env.
///
/// # Safety
/// The raw pointer fields in `SpawnArgs` must point to valid memory.
pub unsafe fn spawn(args: &SpawnArgs) -> Result<Pid, SyscallError> {
    check(syscall(SYS_SPAWN, args as *const SpawnArgs as u64, 0, 0, 0))
        .map(|pid| Pid(pid as u32))
}

/// Wait for process to exit. Returns exit code (blocking).
pub fn waitpid(pid: Pid) -> u64 {
    syscall(SYS_WAITPID, pid.0 as u64, 0, 0, 0)
}

/// Wait for process with flags. Returns exit code, or `Err(WouldBlock)` with WNOHANG
/// if the child has not exited yet.
pub fn waitpid_flags(pid: Pid, flags: u64) -> Result<u64, SyscallError> {
    check(syscall(SYS_WAITPID, pid.0 as u64, flags, 0, 0))
}

/// Mark file descriptor as the controlling TTY for this process.
pub fn mark_tty(fd: Fd) {
    syscall(SYS_MARK_TTY, fd.0 as u64, 0, 0, 0);
}

// --- Threads ---

/// Spawn a new thread with the given entry point, stack pointer, argument, and stack base.
/// `stack_base` is the bottom of the user stack (for stack info queries).
///
/// # Safety
/// `entry` must be a valid function pointer and `stack`/`stack_base` must
/// describe a valid, correctly-sized stack region.
pub unsafe fn thread_spawn(entry: u64, stack: u64, arg: u64, stack_base: u64) -> u64 {
    syscall(SYS_THREAD_SPAWN, entry, stack, arg, stack_base)
}

/// Wait for thread `tid` to exit.
pub fn thread_join(tid: u64) -> u64 {
    syscall(SYS_THREAD_JOIN, tid, 0, 0, 0)
}

/// Set the name of the calling thread (up to 28 bytes, truncated).
pub fn set_thread_name(name: &[u8]) {
    syscall(SYS_SET_THREAD_NAME, name.as_ptr() as u64, name.len() as u64, 0, 0);
}

// --- IPC ---

// --- Filesystem ---

/// Open a file.
pub fn open(path: &[u8], flags: OpenFlags) -> Result<Fd, SyscallError> {
    check(syscall(SYS_OPEN, path.as_ptr() as u64, path.len() as u64, flags.0, 0)).map(|v| Fd(v as i32))
}

/// Close a file descriptor.
pub fn close(fd: Fd) {
    syscall(SYS_CLOSE, fd.0 as u64, 0, 0, 0);
}

/// Seek within a file descriptor. Returns new offset.
pub fn seek(fd: Fd, pos: SeekFrom) -> Result<u64, SyscallError> {
    let (offset, whence) = match pos {
        SeekFrom::Start(n) => (n as i64, 0u64),
        SeekFrom::Current(n) => (n, 1u64),
        SeekFrom::End(n) => (n, 2u64),
    };
    check(syscall(SYS_SEEK, fd.0 as u64, offset as u64, whence, 0))
}

/// Get file metadata for a file descriptor.
pub fn fstat(fd: Fd) -> Result<Stat, SyscallError> {
    let mut stat = Stat { file_type: FileType::Unknown, size: 0, mtime: 0 };
    check_unit(syscall(SYS_FSTAT, fd.0 as u64, &mut stat as *mut Stat as u64, 0, 0))?;
    Ok(stat)
}

/// Flush file descriptor to disk.
pub fn fsync(fd: Fd) -> Result<(), SyscallError> {
    check_unit(syscall(SYS_FSYNC, fd.0 as u64, 0, 0, 0))
}

/// Read directory entries. Returns bytes written to `buf`.
pub fn readdir(path: &[u8], buf: &mut [u8]) -> usize {
    let n = syscall(SYS_READDIR, path.as_ptr() as u64, path.len() as u64, buf.as_mut_ptr() as u64, buf.len() as u64);
    if SyscallError::from_u64(n).is_some() { 0 } else { n as usize }
}

/// Delete a file or directory.
pub fn delete(path: &[u8]) -> Result<(), SyscallError> {
    check_unit(syscall(SYS_DELETE, path.as_ptr() as u64, path.len() as u64, 0, 0))
}

/// Change current working directory.
pub fn chdir(path: &[u8]) -> Result<(), SyscallError> {
    check_unit(syscall(SYS_CHDIR, path.as_ptr() as u64, path.len() as u64, 0, 0))
}

/// Get current working directory. Returns bytes written to `buf`.
pub fn getcwd(buf: &mut [u8]) -> usize {
    let n = syscall(SYS_GETCWD, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0);
    if SyscallError::from_u64(n).is_some() { 0 } else { n as usize }
}

// --- Random ---

/// Fill `buf` with cryptographically secure random bytes.
pub fn random(buf: &mut [u8]) {
    syscall(SYS_RANDOM, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0);
}

// --- Clock ---

/// Nanoseconds since boot (monotonic clock).
pub fn clock_nanos() -> u64 {
    syscall(SYS_CLOCK, 0, 0, 0, 0)
}

/// Read wall-clock time from RTC.
pub fn clock_realtime() -> RealTime {
    let raw = syscall(SYS_CLOCK_REALTIME, 0, 0, 0, 0);
    RealTime {
        hours: ((raw >> 16) & 0xFF) as u8,
        minutes: ((raw >> 8) & 0xFF) as u8,
        seconds: (raw & 0xFF) as u8,
    }
}

/// Seconds since Unix epoch (1970-01-01 00:00:00 UTC), read from CMOS RTC.
pub fn clock_epoch() -> u64 {
    syscall(SYS_CLOCK_EPOCH, 0, 0, 0, 0)
}

// --- Screen / GPU ---

/// Get the screen size as (rows, columns).
pub fn screen_size() -> (usize, usize) {
    let raw = syscall(SYS_SCREEN_SIZE, 0, 0, 0, 0);
    ((raw >> 32) as usize, (raw & 0xFFFF_FFFF) as usize)
}

/// Set the screen size from pixel dimensions (width, height).
pub fn set_screen_size(width: u32, height: u32) {
    syscall(SYS_SET_SCREEN_SIZE, width as u64, height as u64, 0, 0);
}

/// Transfer a region of the framebuffer to the GPU and flush it.
/// Pass (0, 0, 0, 0) to flush the full screen.
pub fn gpu_present(x: u32, y: u32, w: u32, h: u32) {
    syscall(SYS_GPU_PRESENT, x as u64, y as u64, w as u64, h as u64);
}

/// Upload the cursor image from backing and enable hardware cursor.
pub fn gpu_set_cursor(hot_x: u32, hot_y: u32) {
    syscall(SYS_GPU_SET_CURSOR, hot_x as u64, hot_y as u64, 0, 0);
}

/// Move the hardware cursor to screen position (x, y).
pub fn gpu_move_cursor(x: u32, y: u32) {
    syscall(SYS_GPU_MOVE_CURSOR, x as u64, y as u64, 0, 0);
}

/// Request a GPU resolution change. On success, writes the new
/// [`FramebufferInfo`](crate::FramebufferInfo) to `info_out`.
///
/// # Safety
/// `info_out` must point to a writable buffer of at least
/// `size_of::<FramebufferInfo>()` bytes.
pub unsafe fn gpu_set_resolution(width: u32, height: u32, info_out: *mut u8) -> Result<(), SyscallError> {
    check_unit(syscall(SYS_GPU_SET_RESOLUTION, width as u64, height as u64, info_out as u64, 0))
}

/// Set the active keyboard layout by name. Returns `true` on success.
pub fn set_keyboard_layout(name: &str) -> Result<(), SyscallError> {
    check_unit(syscall(SYS_SET_KEYBOARD_LAYOUT, name.as_ptr() as u64, name.len() as u64, 0, 0))
}

/// Shut down the machine. Does not return.
pub fn shutdown() -> ! {
    syscall(SYS_SHUTDOWN, 0, 0, 0, 0);
    loop {}
}

// --- Devices ---

/// Device types for [`open_device`].
#[repr(u64)]
#[derive(Debug, Clone, Copy)]
pub enum DeviceType {
    Keyboard = 0,
    Mouse = 1,
    Framebuffer = 2,
    Nic = 3,
    Audio = 4,
}

/// Claim exclusive access to a device.
pub fn open_device(device: DeviceType) -> Result<Fd, SyscallError> {
    check(syscall(SYS_OPEN_DEVICE, device as u64, 0, 0, 0)).map(|v| Fd(v as i32))
}

// --- Service IPC (listen / accept / connect) ---

/// Register a named service and return a listener fd.
/// Other processes can connect to this service by name.
pub fn listen(name: &str) -> Result<Fd, SyscallError> {
    check(syscall(SYS_LISTEN, name.as_ptr() as u64, name.len() as u64, 0, 0)).map(|v| Fd(v as i32))
}

/// Result of [`accept`]: socket fd + connecting client's PID.
pub struct AcceptResult {
    pub fd: Fd,
    pub client_pid: u32,
}

/// Accept a pending connection on a listener fd.
/// Blocks until a client connects. Returns a socket fd and the client's PID.
pub fn accept(listener_fd: Fd) -> Result<AcceptResult, SyscallError> {
    let raw = syscall(SYS_ACCEPT, listener_fd.0 as u64, 0, 0, 0);
    if let Some(e) = SyscallError::from_u64(raw) {
        return Err(e);
    }
    Ok(AcceptResult {
        fd: Fd((raw & 0xFFFF_FFFF) as i32),
        client_pid: (raw >> 32) as u32,
    })
}

/// Connect to a named service. Blocks until the server accepts.
/// Returns a bidirectional socket fd.
pub fn connect(name: &str) -> Result<Fd, SyscallError> {
    check(syscall(SYS_CONNECT, name.as_ptr() as u64, name.len() as u64, 0, 0)).map(|v| Fd(v as i32))
}

// --- Shared memory ---

/// Allocate a 2MB-aligned shared memory region. Returns an opaque token.
pub fn alloc_shared(size: usize) -> u32 {
    let token = syscall(SYS_ALLOC_SHARED, size as u64, 0, 0, 0);
    assert!(SyscallError::from_u64(token).is_none(), "alloc_shared failed");
    token as u32
}

/// Grant another process permission to map a shared memory region.
pub fn grant_shared(token: u32, target_pid: Pid) {
    let result = syscall(SYS_GRANT_SHARED, token as u64, target_pid.0 as u64, 0, 0);
    assert_eq!(result, 0, "grant_shared failed");
}

/// Map a shared memory region into this process's address space.
///
/// # Safety
/// Caller must ensure the token is valid and manage the returned pointer.
pub unsafe fn map_shared(token: u32) -> *mut u8 {
    let addr = syscall(SYS_MAP_SHARED, token as u64, 0, 0, 0);
    assert!(SyscallError::from_u64(addr).is_none(), "map_shared failed");
    core::ptr::with_exposed_provenance_mut(addr as usize)
}

/// Release this process's mapping of a shared memory region.
pub fn release_shared(token: u32) {
    let result = syscall(SYS_RELEASE_SHARED, token as u64, 0, 0, 0);
    assert_eq!(result, 0, "release_shared failed");
}

// --- System info ---

/// Query system information (memory, CPUs, processes).
/// Returns the number of bytes written to `buf`.
pub fn sysinfo(buf: &mut [u8]) -> usize {
    let n = syscall(SYS_SYSINFO, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0);
    if SyscallError::from_u64(n).is_some() { 0 } else { n as usize }
}

// --- Networking ---

/// Get the MAC address of the network interface.
pub fn net_mac() -> Option<[u8; 6]> {
    let mut buf = [0u8; 6];
    let r = syscall(SYS_NET_INFO, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0);
    if SyscallError::from_u64(r).is_some() { None } else { Some(buf) }
}

/// Send a raw Ethernet frame.
pub fn net_send(frame: &[u8]) {
    syscall(SYS_NET_SEND, frame.as_ptr() as u64, frame.len() as u64, 0, 0);
}

/// Receive a raw Ethernet frame. Blocks until a frame arrives.
pub fn net_recv(buf: &mut [u8]) -> usize {
    syscall(SYS_NET_RECV, buf.as_mut_ptr() as u64, buf.len() as u64, encode_timeout(None), 0) as usize
}

/// Receive a raw Ethernet frame with a timeout.
/// `None` = block forever, `Some(nanos)` = timeout. Returns 0 on timeout.
pub fn net_recv_timeout(buf: &mut [u8], timeout: Option<u64>) -> usize {
    syscall(SYS_NET_RECV, buf.as_mut_ptr() as u64, buf.len() as u64, encode_timeout(timeout), 0) as usize
}

// --- Process / OS ---

/// Sleep for the given number of nanoseconds.
pub fn nanosleep(nanos: u64) {
    syscall(SYS_NANOSLEEP, nanos, 0, 0, 0);
}

/// Duplicate a file descriptor.
pub fn dup(fd: Fd) -> Result<Fd, SyscallError> {
    check(syscall(SYS_DUP, fd.0 as u64, 0, 0, 0)).map(|v| Fd(v as i32))
}

/// Duplicate a file descriptor to a specific fd number.
/// If `new_fd` is already open, it is closed first.
pub fn dup2(old_fd: Fd, new_fd: Fd) -> Result<Fd, SyscallError> {
    check(syscall(SYS_DUP2, old_fd.0 as u64, new_fd.0 as u64, 0, 0)).map(|v| Fd(v as i32))
}

/// Get the current process ID.
pub fn getpid() -> Pid {
    Pid(syscall(SYS_GETPID, 0, 0, 0, 0) as u32)
}

/// Rename a file.
pub fn rename(old: &[u8], new: &[u8]) -> Result<(), SyscallError> {
    check_unit(syscall(SYS_RENAME, old.as_ptr() as u64, old.len() as u64, new.as_ptr() as u64, new.len() as u64))
}

/// Create a directory.
pub fn mkdir(path: &[u8]) -> Result<(), SyscallError> {
    check_unit(syscall(SYS_MKDIR, path.as_ptr() as u64, path.len() as u64, 0, 0))
}

/// Remove a directory.
pub fn rmdir(path: &[u8]) -> Result<(), SyscallError> {
    check_unit(syscall(SYS_RMDIR, path.as_ptr() as u64, path.len() as u64, 0, 0))
}

/// Create a symbolic link at `link` pointing to `target`.
pub fn symlink(target: &[u8], link: &[u8]) -> Result<(), SyscallError> {
    check_unit(syscall(SYS_SYMLINK, target.as_ptr() as u64, target.len() as u64, link.as_ptr() as u64, link.len() as u64))
}

/// Read the target of a symbolic link. Returns the number of bytes written to `buf`.
pub fn readlink(path: &[u8], buf: &mut [u8]) -> Result<usize, SyscallError> {
    check(syscall(SYS_READLINK, path.as_ptr() as u64, path.len() as u64, buf.as_mut_ptr() as u64, buf.len() as u64)).map(|n| n as usize)
}

// --- Dynamic linking ---

/// Load a shared library (.so) into the current process.
/// Runs .init_array constructors after loading.
pub fn dl_open(path: &[u8]) -> Result<u64, SyscallError> {
    let mut init_info: [u64; 2] = [0; 2];
    let handle = check(syscall(SYS_DLOPEN, path.as_ptr() as u64, path.len() as u64, init_info.as_mut_ptr() as u64, 0))?;
    // Run .init_array constructors (e.g. EH frame finder registration in cdylib std)
    let init_array_ptr = init_info[0];
    let init_count = init_info[1];
    if init_array_ptr != 0 && init_count > 0 {
        let entries = unsafe { core::slice::from_raw_parts(init_array_ptr as *const usize, init_count as usize) };
        for &entry in entries {
            if entry != 0 {
                let f: extern "C" fn() = unsafe { core::mem::transmute(entry) };
                f();
            }
        }
    }
    Ok(handle)
}

/// Look up a symbol in a loaded shared library. Returns the address.
///
/// # Safety
/// The returned address must only be transmuted to the correct function signature.
pub unsafe fn dl_sym(handle: u64, name: &[u8]) -> Result<u64, SyscallError> {
    check(syscall(SYS_DLSYM, handle, name.as_ptr() as u64, name.len() as u64, 0))
}

/// Close a loaded shared library handle.
pub fn dl_close(handle: u64) -> u64 {
    syscall(SYS_DLCLOSE, handle, 0, 0, 0)
}

// --- Futex ---

/// Block if `*addr == expected`. Returns 0 on wake, 1 on timeout.
/// `None` = wait forever, `Some(nanos)` = timeout.
///
/// # Safety
/// `addr` must point to a valid, aligned `u32`.
pub unsafe fn futex_wait(addr: *const u32, expected: u32, timeout: Option<u64>) -> u64 {
    syscall(SYS_FUTEX_WAIT, addr as u64, expected as u64, encode_timeout(timeout), 0)
}

/// Wake up to `count` threads waiting on `addr`. Returns number of threads woken.
///
/// # Safety
/// `addr` must point to a valid, aligned `u32`.
pub unsafe fn futex_wake(addr: *const u32, count: u32) -> u64 {
    syscall(SYS_FUTEX_WAKE, addr as u64, count as u64, 0, 0)
}

// --- File truncate ---

/// Truncate file descriptor to `size` bytes.
pub fn ftruncate(fd: Fd, size: u64) -> Result<(), SyscallError> {
    check_unit(syscall(SYS_FTRUNCATE, fd.0 as u64, size, 0, 0))
}

// --- Stack info ---

/// Get the current thread's stack base address and size.
pub fn stack_info() -> Option<(u64, u64)> {
    let mut base: u64 = 0;
    let mut size: u64 = 0;
    let r = syscall(SYS_STACK_INFO, &mut base as *mut u64 as u64, &mut size as *mut u64 as u64, 0, 0);
    if SyscallError::from_u64(r).is_some() { None } else { Some((base, size)) }
}

// --- CPU count ---

/// Return the number of available CPUs.
pub fn cpu_count() -> u32 {
    syscall(SYS_CPU_COUNT, 0, 0, 0, 0) as u32
}

// --- Memory mapping ---

/// Map anonymous memory. Returns pointer on success, null on failure.
///
/// If `addr` is non-null and `flags` includes `MmapFlags::FIXED`, the mapping
/// is placed at exactly that address (must be 2MB-aligned).
/// If `addr` is null, the kernel chooses the address.
///
/// # Safety
/// Caller is responsible for managing the returned memory region.
pub unsafe fn mmap(addr: *mut u8, size: usize, prot: MmapProt, flags: MmapFlags) -> *mut u8 {
    let result = syscall(SYS_MMAP, addr as u64, size as u64, prot.0, flags.0);
    if SyscallError::from_u64(result).is_some() { core::ptr::null_mut() } else {
        core::ptr::with_exposed_provenance_mut(result as usize)
    }
}

/// Unmap a previously mapped region.
///
/// # Safety
/// `addr` and `size` must describe a region previously returned by `mmap`.
pub unsafe fn munmap(addr: *mut u8, size: usize) -> Result<(), SyscallError> {
    check_unit(syscall(SYS_MUNMAP, addr as u64, size as u64, 0, 0))
}

// --- Kill ---

/// Terminate a child process.
pub fn kill(pid: Pid) -> Result<(), SyscallError> {
    check_unit(syscall(SYS_KILL, pid.0 as u64, 0, 0, 0))
}

// --- Non-blocking I/O ---

/// Non-blocking read. Returns bytes read, or `Err(WouldBlock)` if no data available.
pub fn read_nonblock(fd: Fd, buf: &mut [u8]) -> Result<usize, SyscallError> {
    check(syscall(SYS_READ_NONBLOCK, fd.0 as u64, buf.as_mut_ptr() as u64, buf.len() as u64, 0)).map(|n| n as usize)
}

/// Non-blocking write. Returns bytes written, or `Err(WouldBlock)` if no space available.
pub fn write_nonblock(fd: Fd, buf: &[u8]) -> Result<usize, SyscallError> {
    check(syscall(SYS_WRITE_NONBLOCK, fd.0 as u64, buf.as_ptr() as u64, buf.len() as u64, 0)).map(|n| n as usize)
}

// --- Pipe operations ---

/// Open an existing pipe by internal ID. `mode`: 0 = read, 1 = write.
/// Returns a new file descriptor for the pipe.
pub fn pipe_open(pipe_id: u64, mode: u64) -> Result<Fd, SyscallError> {
    check(syscall(SYS_PIPE_OPEN, pipe_id, mode, 0, 0)).map(|v| Fd(v as i32))
}

/// Get the internal pipe ID for a pipe/tty file descriptor.
/// Used to share pipe access across processes via `pipe_open`.
pub fn pipe_id(fd: Fd) -> Result<u64, SyscallError> {
    check(syscall(SYS_PIPE_ID, fd.0 as u64, 0, 0, 0))
}

/// Create a socket file descriptor from two pipe IDs (rx for reading, tx for writing).
/// The kernel bumps refcounts on both pipes. Caller should close original pipe fds after this.
pub fn socket_create(rx_pipe_id: u64, tx_pipe_id: u64) -> Result<Fd, SyscallError> {
    check(syscall(SYS_SOCKET_CREATE, rx_pipe_id, tx_pipe_id, 0, 0)).map(|v| Fd(v as i32))
}

/// Map a pipe's shared-memory ring buffer into this process's address space.
/// Returns a pointer to the `RingHeader` at the start of the mapped region.
pub fn pipe_map(fd: Fd) -> Result<*mut u8, SyscallError> {
    check(syscall(SYS_PIPE_MAP, fd.0 as u64, 0, 0, 0)).map(|v| v as *mut u8)
}

// --- NIC DMA control ---

/// Poll for a received frame. Returns `(buf_index << 16) | frame_len`, or 0 if none.
pub fn nic_rx_poll() -> u64 {
    syscall(SYS_NIC_RX_POLL, 0, 0, 0, 0)
}

/// Tell the kernel to refill RX buffer `buf_index` after consuming the frame.
pub fn nic_rx_done(buf_index: u64) {
    syscall(SYS_NIC_RX_DONE, buf_index, 0, 0, 0);
}

/// Submit the TX DMA buffer to hardware. `total_len` includes the net header.
pub fn nic_tx(total_len: u64) {
    syscall(SYS_NIC_TX, total_len, 0, 0, 0);
}

// --- Audio ---

/// Submit a filled DMA buffer to the audio device.
/// `buf_idx`: index of the DMA buffer (0..num_buffers).
/// `len`: number of bytes of PCM data written to the buffer.
pub fn audio_submit(buf_idx: u32, len: u32) {
    syscall(SYS_AUDIO_SUBMIT, buf_idx as u64, len as u64, 0, 0);
}

/// Allocate a TLS block for a dlopen'd module on the current thread.
/// Returns the physical address of the allocated block (as stored in the DTV).
/// Panics in the kernel if module_id is invalid or allocation fails.
pub fn tls_alloc_block(module_id: u64) -> u64 {
    syscall(SYS_TLS_ALLOC_BLOCK, module_id, 0, 0, 0)
}

// --- io_uring ---

/// Create an io_uring instance with the given queue depth (must be power of 2, max 256).
/// Returns (ring_fd, shared_memory_token). The shared memory contains the SQ/CQ rings
/// and SQE array; map it with `map_shared()` to access them.
pub fn io_uring_setup(depth: u32) -> Result<(Fd, u32), SyscallError> {
    let raw = check(syscall(SYS_IO_URING_SETUP, depth as u64, 0, 0, 0))?;
    let fd = Fd((raw & 0xFFFF_FFFF) as i32);
    let token = (raw >> 32) as u32;
    Ok((fd, token))
}

/// Submit SQEs and/or wait for completions on an io_uring instance.
/// `to_submit`: number of SQEs to consume from the SQ ring.
/// `min_complete`: block until at least this many CQEs are available (0 = don't block).
/// `timeout_nanos`: 0 = non-blocking, u64::MAX = block forever, else timeout in nanos.
/// Returns the number of CQEs available.
pub fn io_uring_enter(fd: Fd, to_submit: u32, min_complete: u32, timeout_nanos: u64) -> Result<u32, SyscallError> {
    check(syscall(SYS_IO_URING_ENTER, fd.0 as u64, to_submit as u64, min_complete as u64, timeout_nanos))
        .map(|n| n as u32)
}

// --- Module info (for stack unwinding / backtraces) ---

/// Information about a loaded module (executable or shared library).
///
/// Buffer layout returned by `SYS_QUERY_MODULES`:
///   `[ModuleInfo; count]` followed by packed path strings.
///   Each `ModuleInfo::path_offset` is relative to the start of the buffer.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ModuleInfo {
    /// Load base address (bias) of this module.
    pub base: u64,
    /// End of the last mapped segment (base + vaddr_max).
    pub text_end: u64,
    /// Absolute virtual address of `.eh_frame_hdr` (0 if none).
    pub eh_frame_hdr: u64,
    /// Size of `.eh_frame_hdr` in bytes.
    pub eh_frame_hdr_size: u64,
    /// Byte offset of the module's path string within the buffer.
    pub path_offset: u32,
    /// Length of the path string in bytes.
    pub path_len: u32,
}

/// Query all loaded modules (exe + dlopen'd libs) in the current process.
///
/// Writes an array of `ModuleInfo` followed by path strings into `buf`.
/// Returns the number of modules on success.
/// Returns `Err(InvalidArgument)` with the required buffer size encoded
/// if `buf` is too small.
pub fn query_modules(buf: &mut [u8]) -> Result<usize, SyscallError> {
    check(syscall(SYS_QUERY_MODULES, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0)).map(|n| n as usize)
}

// --- Scheduler introspection ---

/// Scheduler info for the calling process.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct SchedInfo {
    /// Current vruntime of this process (nanoseconds of virtual CPU time).
    pub vruntime: u64,
    /// Global min_vruntime frontier (minimum vruntime across all runnable processes).
    pub min_vruntime: u64,
}

/// Get scheduler info for the calling process.
pub fn sched_info() -> SchedInfo {
    let mut info = SchedInfo { vruntime: 0, min_vruntime: 0 };
    syscall(SYS_SCHED_INFO, &mut info as *mut SchedInfo as u64, 0, 0, 0);
    info
}

/// Per-process accounting statistics. Used as the snapshot stashed on the parent
/// at process exit and returned by SYS_PROCESS_STATS.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ProcessStats {
    pub wall_ns: u64,
    pub cpu_ns: u64,
    pub syscall_total: u64,
    pub syscall_total_ns: u64,
    pub fault_demand_count: u32,
    pub fault_zero_count: u32,
    pub fault_ns: u64,
    pub io_read_ops: u32,
    pub _pad: u32,
    pub io_read_bytes: u64,
    pub blocked_io_ns: u64,
    pub blocked_futex_ns: u64,
    pub blocked_pipe_ns: u64,
    pub blocked_ipc_ns: u64,
    pub blocked_other_ns: u64,
    pub runqueue_wait_ns: u64,
    pub peak_memory: u64,
    pub alloc_count: u64,
}

/// Read accounting stats for an exited child process.
/// Returns Ok(()) on success, Err if no stats available for that pid.
pub fn process_stats(child_pid: Pid, stats: &mut ProcessStats) -> Result<(), SyscallError> {
    check_unit(syscall(
        SYS_PROCESS_STATS,
        child_pid.0 as u64,
        stats as *mut ProcessStats as u64,
        core::mem::size_of::<ProcessStats>() as u64,
        0,
    ))
}
