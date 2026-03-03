// Syscall numbers (must match kernel dispatch table)
pub const SYS_WRITE: u64 = 0;
pub const SYS_READ: u64 = 1;
pub const SYS_ALLOC: u64 = 2;
pub const SYS_FREE: u64 = 3;
pub const SYS_REALLOC: u64 = 4;
pub const SYS_EXIT: u64 = 5;
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
pub const SYS_POLL: u64 = 27;
pub const SYS_MARK_TTY: u64 = 28;
pub const SYS_SEND_MSG: u64 = 29;
pub const SYS_RECV_MSG: u64 = 30;
pub const SYS_OPEN_DEVICE: u64 = 31;
pub const SYS_REGISTER_NAME: u64 = 32;
pub const SYS_FIND_PID: u64 = 33;
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

// ---------------------------------------------------------------------------
// Wrappers
// ---------------------------------------------------------------------------

// --- I/O ---

/// Write `len` bytes from `buf` to file descriptor `fd`. Returns bytes written.
pub fn write(fd: u64, buf: *const u8, len: usize) -> u64 {
    syscall(SYS_WRITE, fd, buf as u64, len as u64, 0)
}

/// Read up to `len` bytes from file descriptor `fd` into `buf`. Returns bytes read.
pub fn read(fd: u64, buf: *mut u8, len: usize) -> u64 {
    syscall(SYS_READ, fd, buf as u64, len as u64, 0)
}

/// Read from a file descriptor into a slice. Returns bytes read.
pub fn read_fd(fd: u64, buf: &mut [u8]) -> usize {
    syscall(SYS_READ, fd, buf.as_mut_ptr() as u64, buf.len() as u64, 0) as usize
}

// --- Memory ---

/// Allocate `size` bytes with `align` alignment.
pub fn alloc(size: usize, align: usize) -> *mut u8 {
    core::ptr::with_exposed_provenance_mut(syscall(SYS_ALLOC, size as u64, align as u64, 0, 0) as usize)
}

/// Free an allocation at `ptr` with original `size` and `align`.
pub fn free(ptr: *mut u8, size: usize, align: usize) {
    syscall(SYS_FREE, ptr as u64, size as u64, align as u64, 0);
}

/// Reallocate `ptr` (with original `size`/`align`) to `new_size`.
pub fn realloc(ptr: *mut u8, size: usize, align: usize, new_size: usize) -> *mut u8 {
    core::ptr::with_exposed_provenance_mut(syscall(SYS_REALLOC, ptr as u64, size as u64, align as u64, new_size as u64) as usize)
}

// --- Process ---

/// Exit the process with `code`. Does not return.
pub fn exit(code: i32) -> ! {
    loop { syscall(SYS_EXIT, code as u64, 0, 0, 0); }
}

/// Create a pipe. Returns packed (read_fd, write_fd) as u64.
pub fn pipe() -> u64 {
    syscall(SYS_PIPE, 0, 0, 0, 0)
}

/// Spawn a new process. Returns pid on success, u64::MAX on failure.
pub fn spawn(argv: *const u8, len: usize, fd_map: *const [u32; 2], fd_map_count: usize) -> u64 {
    syscall(SYS_SPAWN, argv as u64, len as u64, fd_map as u64, fd_map_count as u64)
}

/// Wait for process `pid` to exit. Returns exit code.
pub fn waitpid(pid: u64) -> u64 {
    syscall(SYS_WAITPID, pid, 0, 0, 0)
}

/// Mark file descriptor `fd` as the controlling TTY for this process.
pub fn mark_tty(fd: u64) -> u64 {
    syscall(SYS_MARK_TTY, fd, 0, 0, 0)
}

// --- Threads ---

/// Spawn a new thread with the given entry point, stack pointer, and argument.
pub fn thread_spawn(entry: u64, stack: u64, arg: u64) -> u64 {
    syscall(SYS_THREAD_SPAWN, entry, stack, arg, 0)
}

/// Wait for thread `tid` to exit.
pub fn thread_join(tid: u64) -> u64 {
    syscall(SYS_THREAD_JOIN, tid, 0, 0, 0)
}

// --- IPC ---

/// Send a message to process `target_pid`.
pub fn send_msg(target_pid: u64, msg_ptr: u64) -> u64 {
    syscall(SYS_SEND_MSG, target_pid, msg_ptr, 0, 0)
}

/// Receive a message into the buffer at `msg_ptr`.
pub fn recv_msg(msg_ptr: u64) -> u64 {
    syscall(SYS_RECV_MSG, msg_ptr, 0, 0, 0)
}

// --- Filesystem ---

/// Open a file. Returns fd on success, u64::MAX on failure.
pub fn open(path: *const u8, path_len: usize, flags: u64) -> u64 {
    syscall(SYS_OPEN, path as u64, path_len as u64, flags, 0)
}

/// Close a file descriptor.
pub fn close(fd: u64) -> u64 {
    syscall(SYS_CLOSE, fd, 0, 0, 0)
}

/// Seek within a file descriptor. Returns new offset.
pub fn seek(fd: u64, offset: i64, whence: u64) -> u64 {
    syscall(SYS_SEEK, fd, offset as u64, whence, 0)
}

/// Get file size for a file descriptor.
pub fn fstat(fd: u64) -> u64 {
    syscall(SYS_FSTAT, fd, 0, 0, 0)
}

/// Flush file descriptor to disk.
pub fn fsync(fd: u64) -> u64 {
    syscall(SYS_FSYNC, fd, 0, 0, 0)
}

/// Read directory entries. Returns bytes written to `buf`.
pub fn readdir(path: *const u8, path_len: usize, buf: *mut u8, buf_len: usize) -> u64 {
    syscall(SYS_READDIR, path as u64, path_len as u64, buf as u64, buf_len as u64)
}

/// Delete a file or directory.
pub fn delete(path: *const u8, path_len: usize) -> u64 {
    syscall(SYS_DELETE, path as u64, path_len as u64, 0, 0)
}

/// Change current working directory.
pub fn chdir(path: *const u8, path_len: usize) -> u64 {
    syscall(SYS_CHDIR, path as u64, path_len as u64, 0, 0)
}

/// Get current working directory. Returns bytes written to `buf`.
pub fn getcwd(buf: *mut u8, buf_len: usize) -> u64 {
    syscall(SYS_GETCWD, buf as u64, buf_len as u64, 0, 0)
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
/// Returns packed: (hours << 16) | (minutes << 8) | seconds.
pub fn clock_realtime() -> u64 {
    syscall(SYS_CLOCK_REALTIME, 0, 0, 0, 0)
}

// --- Screen / GPU ---

/// Get the screen size as (rows, columns).
pub fn screen_size() -> (usize, usize) {
    let raw = syscall(SYS_SCREEN_SIZE, 0, 0, 0, 0);
    ((raw >> 32) as usize, (raw & 0xFFFF_FFFF) as usize)
}

/// Set the screen size from pixel dimensions (width, height).
/// The kernel computes rows/columns assuming an 8x16 font.
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

/// Set the active keyboard layout by name. Returns `true` on success.
pub fn set_keyboard_layout(name: &str) -> bool {
    syscall(SYS_SET_KEYBOARD_LAYOUT, name.as_ptr() as u64, name.len() as u64, 0, 0) != 0
}

/// Shut down the machine. Does not return.
pub fn shutdown() -> ! {
    syscall(SYS_SHUTDOWN, 0, 0, 0, 0);
    loop {}
}

// --- Poll ---

/// Result of a [`poll`] or [`poll_timeout`] call.
pub struct PollResult {
    mask: u64,
    fd_count: usize,
}

impl PollResult {
    /// Whether the file descriptor at `index` is ready.
    pub fn fd(&self, index: usize) -> bool {
        self.mask & (1 << index) != 0
    }

    /// Whether the process message queue has messages.
    pub fn messages(&self) -> bool {
        self.mask & (1 << self.fd_count) != 0
    }
}

/// Poll file descriptors and the message queue for readiness.
/// Blocks until at least one source has data.
pub fn poll(fds: &[u64]) -> PollResult {
    poll_timeout(fds, 0)
}

/// Poll file descriptors and the message queue for readiness.
/// Returns when at least one source has data, or after `timeout_nanos`
/// nanoseconds (whichever comes first). Pass 0 to block indefinitely.
pub fn poll_timeout(fds: &[u64], timeout_nanos: u64) -> PollResult {
    let mask = syscall(SYS_POLL, fds.as_ptr() as u64, fds.len() as u64, timeout_nanos, 0);
    PollResult { mask, fd_count: fds.len() }
}

// --- Devices ---

/// Device types for [`open_device`].
#[repr(u64)]
#[derive(Debug, Clone, Copy)]
pub enum DeviceType {
    Keyboard = 0,
    Mouse = 1,
    Framebuffer = 2,
}

/// Claim exclusive access to a device. Returns the FD number on success.
/// Fails if the device is already claimed by another process.
pub fn open_device(device: DeviceType) -> Option<u64> {
    let fd = syscall(SYS_OPEN_DEVICE, device as u64, 0, 0, 0);
    if fd == u64::MAX { None } else { Some(fd) }
}

// --- Name registry ---

/// Error returned when [`register_name`] fails because the name is already registered.
#[derive(Debug)]
pub struct NameTaken;

impl core::fmt::Display for NameTaken {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("name already registered")
    }
}

/// Register the current process under the given name.
/// Fails if the name is already taken by another process.
/// Other processes can discover this process via [`find_pid`].
pub fn register_name(name: &str) -> Result<(), NameTaken> {
    let result = syscall(SYS_REGISTER_NAME, name.as_ptr() as u64, name.len() as u64, 0, 0);
    if result == 0 { Ok(()) } else { Err(NameTaken) }
}

/// Find the PID of a process registered under the given name.
pub fn find_pid(name: &str) -> Option<u32> {
    let pid = syscall(SYS_FIND_PID, name.as_ptr() as u64, name.len() as u64, 0, 0);
    if pid == u64::MAX { None } else { Some(pid as u32) }
}

// --- Shared memory ---

/// Allocate a 2MB-aligned shared memory region. Returns an opaque token.
/// The region is mapped into the caller's address space automatically.
pub fn alloc_shared(size: usize) -> u32 {
    let token = syscall(SYS_ALLOC_SHARED, size as u64, 0, 0, 0);
    assert!(token != u64::MAX, "alloc_shared failed");
    token as u32
}

/// Grant another process permission to map a shared memory region.
pub fn grant_shared(token: u32, target_pid: u32) {
    let result = syscall(SYS_GRANT_SHARED, token as u64, target_pid as u64, 0, 0);
    assert_eq!(result, 0, "grant_shared failed");
}

/// Map a shared memory region into this process's address space.
/// Returns a pointer to the mapped memory.
pub fn map_shared(token: u32) -> *mut u8 {
    let addr = syscall(SYS_MAP_SHARED, token as u64, 0, 0, 0);
    assert!(addr != u64::MAX, "map_shared failed");
    core::ptr::with_exposed_provenance_mut(addr as usize)
}

/// Release this process's mapping of a shared memory region.
/// Unmaps only from the caller's address space. Deallocates when no mappings remain.
pub fn release_shared(token: u32) {
    let result = syscall(SYS_RELEASE_SHARED, token as u64, 0, 0, 0);
    assert_eq!(result, 0, "release_shared failed");
}

// --- System info ---

/// Query system information (memory, CPUs, processes).
/// Fills `buf` with a header followed by per-process entries.
/// Returns the number of bytes written.
pub fn sysinfo(buf: &mut [u8]) -> usize {
    let n = syscall(SYS_SYSINFO, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0);
    if n == u64::MAX { 0 } else { n as usize }
}

// --- Networking ---

/// Get the MAC address of the network interface.
/// Returns `None` if no NIC is present.
pub fn net_mac() -> Option<[u8; 6]> {
    let mut buf = [0u8; 6];
    let r = syscall(SYS_NET_INFO, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0);
    if r == u64::MAX { None } else { Some(buf) }
}

/// Send a raw Ethernet frame.
pub fn net_send(frame: &[u8]) {
    syscall(SYS_NET_SEND, frame.as_ptr() as u64, frame.len() as u64, 0, 0);
}

/// Receive a raw Ethernet frame. Blocks until a frame arrives.
/// Returns the number of bytes written to `buf`.
pub fn net_recv(buf: &mut [u8]) -> usize {
    syscall(SYS_NET_RECV, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0) as usize
}

/// Receive a raw Ethernet frame with a timeout.
/// Returns the number of bytes written, or 0 on timeout.
pub fn net_recv_timeout(buf: &mut [u8], timeout_nanos: u64) -> usize {
    syscall(SYS_NET_RECV, buf.as_mut_ptr() as u64, buf.len() as u64, timeout_nanos, 0) as usize
}

// --- Process / OS ---

/// Sleep for the given number of nanoseconds.
pub fn nanosleep(nanos: u64) {
    syscall(SYS_NANOSLEEP, nanos, 0, 0, 0);
}

/// Duplicate a file descriptor. Returns the new fd, or u64::MAX on failure.
pub fn dup(fd: u64) -> u64 {
    syscall(SYS_DUP, fd, 0, 0, 0)
}

/// Get the current process ID.
pub fn getpid() -> u64 {
    syscall(SYS_GETPID, 0, 0, 0, 0)
}

/// Rename a file. Returns 0 on success, u64::MAX on failure.
pub fn rename(old: *const u8, old_len: usize, new: *const u8, new_len: usize) -> u64 {
    syscall(SYS_RENAME, old as u64, old_len as u64, new as u64, new_len as u64)
}

/// Create a directory. Returns 0 on success, u64::MAX on failure.
pub fn mkdir(path: *const u8, path_len: usize) -> u64 {
    syscall(SYS_MKDIR, path as u64, path_len as u64, 0, 0)
}

/// Remove a directory. Returns 0 on success, u64::MAX on failure.
pub fn rmdir(path: *const u8, path_len: usize) -> u64 {
    syscall(SYS_RMDIR, path as u64, path_len as u64, 0, 0)
}

// --- Dynamic linking ---

/// Load a shared library (.so) into the current process. Returns a handle, or u64::MAX on failure.
pub fn dl_open(path: *const u8, path_len: usize) -> u64 {
    syscall(SYS_DLOPEN, path as u64, path_len as u64, 0, 0)
}

/// Look up a symbol in a loaded shared library. Returns the address, or u64::MAX on failure.
pub fn dl_sym(handle: u64, name: *const u8, name_len: usize) -> u64 {
    syscall(SYS_DLSYM, handle, name as u64, name_len as u64, 0)
}

/// Close a loaded shared library handle.
pub fn dl_close(handle: u64) -> u64 {
    syscall(SYS_DLCLOSE, handle, 0, 0, 0)
}
