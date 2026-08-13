// Syscall numbers (must match kernel dispatch table)
pub const SYS_WRITE: u64 = 0;
pub const SYS_READ: u64 = 1;
// Syscall numbers 2-4 are unused (formerly SYS_ALLOC/FREE/REALLOC).
pub const SYS_THREAD_EXIT: u64 = 5;
pub const SYS_RANDOM: u64 = 6;
// Syscall number 7 unused (formerly SYS_SCREEN_SIZE).
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
// Syscall number 23 unused (formerly SYS_SET_KEYBOARD_LAYOUT: the kernel has
// no layout to set — it delivers key transitions and userland translates).
pub const SYS_PIPE: u64 = 24;
/// Start a program, endowing it exactly what the caller names. Answers a
/// `Process` handle — see [`spawn`].
pub const SYS_SPAWN: u64 = 25;
// Syscall number 26 unused (formerly SYS_WAITPID: a pid is not authority over
// a process, and a pid outlives nothing — the number is reissued. Waiting is
// [`SYS_PROCESS_WAIT`] on the handle the spawn answered with).
pub const SYS_MARK_TTY: u64 = 28;
// Syscall numbers 29-31 unused (formerly SYS_SEND_MSG/SYS_RECV_MSG and
// SYS_OPEN_DEVICE: first-come claiming, where whoever asked first got the
// device. Arbitration is the manifest now — init mints every claim from a
// `SysCap` and endows it, so who holds a device is a fact the image was built
// with rather than a race).
// Syscall numbers 32-33 unused (formerly SYS_REGISTER_NAME/SYS_FIND_PID).
// Syscall number 34 unused (formerly SYS_SET_SCREEN_SIZE).
pub const SYS_GPU_PRESENT: u64 = 35;
// Syscall numbers 36-39 unused (formerly SYS_ALLOC_SHARED, SYS_GRANT_SHARED,
// SYS_MAP_SHARED and SYS_RELEASE_SHARED: a shared-memory token was an id
// treated as a capability and the grant list was a pid ACL. A region is a
// handle now — [`SYS_SHM_CREATE`], [`SYS_SHM_MAP`], [`SYS_SHM_UNMAP`] — and
// giving one away is [`SYS_HANDLE_SEND`]).
pub const SYS_THREAD_SPAWN: u64 = 40;
pub const SYS_THREAD_JOIN: u64 = 41;
pub const SYS_CLOCK_REALTIME: u64 = 42;
pub const SYS_GPU_SET_CURSOR: u64 = 43;
pub const SYS_GPU_MOVE_CURSOR: u64 = 44;
pub const SYS_SYSINFO: u64 = 45;
// Syscall numbers 46-48 unused (formerly SYS_NET_INFO/SYS_NET_SEND/SYS_NET_RECV:
// an ungated frame-copy path that no program ever used — netd drives the NIC
// through its DMA descriptor instead).
pub const SYS_NANOSLEEP: u64 = 49;
/// A second handle to the same object, carrying no more than the first. See
/// [`dup`] and [`dup_narrowed`].
pub const SYS_HANDLE_DUP: u64 = 50;
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
// Syscall number 65 unused (formerly SYS_KILL: pid-addressed, and gated on
// being the target's parent — which is a relationship the kernel happened to
// remember, not a capability anyone was given. [`SYS_PROCESS_KILL`] takes a
// handle carrying `Rights::MANAGE`).
pub const SYS_READ_NONBLOCK: u64 = 66;
pub const SYS_WRITE_NONBLOCK: u64 = 67;
// Syscall numbers 68 and 70 unused (formerly SYS_PIPE_OPEN and SYS_PIPE_ID: a
// pipe id was guessable, and openable by anyone its creator had ever spoken to.
// A pipe end travels as itself now, over [`SYS_HANDLE_SEND`]).
// Syscall numbers 71 and 84 unused (formerly SYS_AUDIO_SUBMIT and
// SYS_AUDIO_POLL: the kernel no longer drives a sound card, so a period is
// published into a ring the kernel built and there is nothing to submit).
// 84 never had a dispatch arm or a caller, so retiring it saves nothing.
pub const SYS_EXIT: u64 = 72;
pub const SYS_GET_ENV: u64 = 73;
/// A second handle to the same object, at a slot the caller picks. See
/// [`dup2`].
pub const SYS_HANDLE_DUP_AT: u64 = 74;
pub const SYS_CLOCK_EPOCH: u64 = 75;
/// Join a pipe read end and a pipe write end into one duplex `Connection`.
/// See [`connection_join`].
///
/// Formerly `SYS_SOCKET_CREATE`, which took two pipe *ids* and is the same
/// operation with the argument the capability model requires — the way
/// [`SYS_HANDLE_DUP`] is `SYS_DUP` with a rights word. What is retired is
/// addressing a pipe by a number anyone could guess, not making a duplex
/// object out of two simplex ends: `std`'s `TcpStream` is one fd, and netd's
/// data path is two pipes.
pub const SYS_CONNECTION_JOIN: u64 = 76;
pub const SYS_PIPE_MAP: u64 = 77;
pub const SYS_NIC_RX_POLL: u64 = 78;
pub const SYS_NIC_RX_DONE: u64 = 79;
pub const SYS_NIC_TX: u64 = 80;
pub const SYS_SYMLINK: u64 = 81;
pub const SYS_READLINK: u64 = 82;
pub const SYS_GPU_SET_RESOLUTION: u64 = 83;
// Syscall number 85 is retired and unused: it was `SYS_LISTEN`, which took a
// service name first-come from a flat global registry. There is no registry;
// a server is endowed an acceptor.
/// Accept a queued connection from an [`Acceptor`] handle.
///
/// [`Acceptor`]: crate::handle::RawHandle
pub const SYS_ACCEPT: u64 = 86;
// Syscall number 87 is retired and unused: it was `SYS_CONNECT`, which
// resolved a name through that registry. A name resolves in a namespace a
// process was given, through `SYS_NAMESPACE_OPEN`, or nowhere.
/// Allocate a TLS block for a dlopen'd module on the current thread.
/// Arg0: module_id (1-based DTV index). Returns the block's virtual address,
/// or a `SyscallError` word — see [`tls_alloc_block`].
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
// Syscall number 96 unused (formerly SYS_SET_RT_PRIORITY: gated on holding a
// sound-device claim, and a claim is not a privilege. [`SYS_RT_ENTER`] is the
// privilege that gate was standing in for).
/// Read one register of a claimed device. See [`device_reg_read`].
pub const SYS_DEVICE_REG_READ: u64 = 97;
/// Write one register of a claimed device. See [`device_reg_write`].
pub const SYS_DEVICE_REG_WRITE: u64 = 98;

/// Read this process's endowment table back: the `(label, handle)` pairs its
/// parent gave it at spawn, as an `[EndowEntry]` count followed by the entries
/// and the label blob they index into. `buf_len == 0` asks how many bytes the
/// answer needs. See [`endowments`].
///
/// The handles themselves are in the table whether or not this is ever called —
/// the labels are *names* for them, not the authority.
pub const SYS_ENDOWMENTS: u64 = 99;

/// Make a port: one [`Acceptor`] for the server, one `Connector` for its
/// clients, packed `(acceptor << 32) | connector`. See [`port_create`].
///
/// **The packing cannot be read as an error, and the reason is slot
/// retirement.** A `SyscallError` encodes as `u64::MAX - code` for
/// `code < 256`, so a pair could collide only if both halves could reach
/// `0xFFFF_FFFF`. A slot at [`RawHandle::MAX_GENERATION`] is retired rather
/// than reissued, so the largest handle any table hands out is `0xFFFF_EFFF`
/// and the largest pair is `0xFFFF_EFFF_FFFF_EFFF` — four billion below the
/// error range. The retirement rule and this packing are load-bearing for each
/// other.
///
/// [`Acceptor`]: port_create
pub const SYS_PORT_CREATE: u64 = 100;
/// Build a namespace from a base and a set of `(name, connector)` additions.
/// See [`NamespaceBuild`].
pub const SYS_NAMESPACE_BUILD: u64 = 101;
/// Open a connection to a name **in a namespace this process holds**. There is
/// no other place to ask, and a name it was not given resolves to nothing.
pub const SYS_NAMESPACE_OPEN: u64 = 102;

/// Move handles to the peer of a connection. See [`handle_send`].
///
/// The batch is queued on the connection, not interleaved with its bytes, so
/// **handles are sent before the frame that announces them** and a receiver
/// that has the frame already has the handles. The SDK's
/// `Connection::send_with_handles` is that ordering written once.
pub const SYS_HANDLE_SEND: u64 = 103;
/// Take the oldest batch of handles the peer sent. See [`handle_recv`].
pub const SYS_HANDLE_RECV: u64 = 104;
/// Make a shared-memory region, sized up to whole 2 MiB pages.
/// See [`shm_create`].
pub const SYS_SHM_CREATE: u64 = 105;
/// Map a region into the caller. Idempotent: a second call answers the first
/// call's address. See [`shm_map`].
pub const SYS_SHM_MAP: u64 = 106;
/// Unmap a region from the caller. See [`shm_unmap`].
pub const SYS_SHM_UNMAP: u64 = 107;

/// Wait for the process a handle names and take its exit code, gated by
/// [`Rights::WAIT`]. See [`process_wait`].
///
/// **An exit code is a property of the object, not of a table entry**, so this
/// answers whether or not the process is still around: a spawner that waits a
/// second time gets the same code, and one that waits for the first time long
/// after the process is gone gets it too. There is nothing to reap and no
/// window in which an exit is missed.
///
/// [`Rights::WAIT`]: crate::handle::Rights::WAIT
pub const SYS_PROCESS_WAIT: u64 = 108;
/// Kill the process a handle names, gated by [`Rights::MANAGE`].
/// See [`process_kill`].
///
/// [`Rights::MANAGE`]: crate::handle::Rights::MANAGE
pub const SYS_PROCESS_KILL: u64 = 109;
/// A `Process` handle for a pid, gated by [`Rights::MANAGE`] on a `SysCap`.
/// See [`process_open`].
///
/// The one place a pid becomes authority, and only `/bin/init` holds a cap that
/// carries the right — so the set of processes that can reach a process they
/// did not start is exactly what init endowed.
///
/// [`Rights::MANAGE`]: crate::handle::Rights::MANAGE
pub const SYS_PROCESS_OPEN: u64 = 110;

/// Mint a device claim for a class, gated by [`Rights::DEVICE`] on a `SysCap`.
/// Only `/bin/init` holds such a cap, so the set of processes that can ever
/// claim a device is exactly what init endowed. See [`device_claim`].
///
/// [`Rights::DEVICE`]: crate::handle::Rights::DEVICE
pub const SYS_DEVICE_CLAIM: u64 = 111;
/// Enter the real-time scheduling band, gated by [`Rights::RT`] on a `SysCap`.
/// A claim is not a privilege; this is. See [`rt_enter`].
///
/// [`Rights::RT`]: crate::handle::Rights::RT
pub const SYS_RT_ENTER: u64 = 112;

// Number 113 is **reserved, not free**: it is `SYS_PORT_REARM`
// (`specs/capability-endowment-spec.md` §7), which mints a fresh `Acceptor`
// for a port whose server died and is the one thing that would make any
// `serves` daemon restartable. Nothing needs it yet, so nothing is built.
//
// 115 is likewise held, for `SYS_SLEEP_UNTIL`
// (`specs/completion-architecture-spec.md`), which replaces a retired
// `SYS_NANOSLEEP`.
//
// **Both are recorded here rather than in those specs alone**, because this
// file is where an agent allocating a number looks and a reservation nobody
// reads is not a reservation. Holes are already ordinary here — a retired
// number is never reused either.

/// Copy kernel log records into a caller's buffer, advancing a cursor the
/// caller owns. Gated by [`Rights::LOG`] on a `SysCap`. See [`log_read`] and
/// [`crate::log`].
///
/// **The kernel keeps no per-reader state**, so a second reader costs nothing
/// and the stream is not consumed: `/bin/logd` and a `log-follow` tool coexist
/// with no coordination. Reading the whole machine's log is authority, which is
/// why it rides a right rather than being ambient.
///
/// [`Rights::LOG`]: crate::handle::Rights::LOG
pub const SYS_LOG_READ: u64 = 114;

/// Bins in the per-process syscall profile — one for every number this ABI
/// issues, and one at the end for every number it does not.
///
/// **The profile's parts sum to its total, and that is the whole requirement.**
/// It was `[u32; 64]` while the ABI reached 98, and the bump was guarded by a
/// silent `if num < len`: every audio, network, IPC and pipe call fell out of
/// the line, 15% of doom's, with the total still counting them.
pub const SYSCALL_PROFILE_BINS: usize = 128;

/// Where a number this ABI does not issue is counted. Merging is a degradation
/// a reader can see in the line; dropping is one nobody can.
pub const SYSCALL_PROFILE_OTHER: usize = SYSCALL_PROFILE_BINS - 1;

const _: () = assert!(SYS_LOG_READ < SYSCALL_PROFILE_OTHER as u64);

pub const WNOHANG: u64 = 1;

/// Arguments for the `SYS_SPAWN` syscall, passed as a single pointer.
///
/// **Two vectors, two verbs.** `slot_map` *duplicates* — the parent keeps its
/// stdout — and `endow` *moves*, so a parent that wants to keep what it endows
/// duplicates first. That is what makes endowing a device claim work with no
/// special case: a claim carries no [`Rights::DUP`], so the move is the only
/// expressible form and the parent provably no longer holds it.
///
/// [`Rights::DUP`]: crate::handle::Rights::DUP
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SpawnArgs {
    pub argv_ptr: u64,
    pub argv_len: u64,
    /// `[[child_slot: u32, parent_handle: RawHandle]]`, duplicated into the
    /// child. Stdio and nothing else, in practice.
    pub slot_map_ptr: u64,
    pub slot_map_count: u64,
    pub env_ptr: u64,
    pub env_len: u64,
    /// `[EndowEntry]`, moved out of the parent's table.
    pub endow_ptr: u64,
    pub endow_count: u64,
    /// The label blob every [`EndowEntry`]'s `label_off`/`label_len` indexes.
    pub labels_ptr: u64,
    pub labels_len: u64,
}

const _: () = assert!(core::mem::size_of::<SpawnArgs>() == 80);

/// One `(label, handle)` pair of a process's endowment table.
///
/// `label_off`/`label_len` index the label blob that travels beside the
/// entries — in [`SpawnArgs`] going in, in [`endowments`]'s answer coming back.
/// The label is a *local name* in one process's own table and buys nothing to
/// guess: a name not in your table resolves to nothing.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct EndowEntry {
    pub label_off: u32,
    pub label_len: u32,
    pub handle: RawHandle,
    /// Named, so nothing leaks kernel stack into it.
    pub _pad: u32,
}

const _: () = assert!(core::mem::size_of::<EndowEntry>() == 16);

/// The label the kernel puts on `/bin/init`'s system capability, and the one
/// init puts on the `RT`-only dup it endows a `realtime` program.
///
/// Here rather than in the SDK because the kernel writes it and userland reads
/// it, and a label spelled twice is a label that can be spelled two ways.
pub const SYSCAP_LABEL: &str = "syscap";
/// The label for a program's namespace — what its manifest `receives` becomes.
pub const SVC_LABEL: &str = "svc";
/// `serve:<name>`: the acceptor of a machine-wide port this program serves.
pub const SERVE_PREFIX: &str = "serve:";
/// `dev:<class>`: the claim for a device class this program was given.
pub const DEV_PREFIX: &str = "dev:";
/// `provide:<name>`: a connector a *launching client* transferred, beside the
/// namespace entry the launcher made from it.
///
/// **Both, and the second is what makes the chain work.** A namespace answers
/// with connections, not with connectors, so a process holding `surface` only
/// in its namespace cannot hand `surface` to a child — and the terminal → shell
/// → `locale` chain is exactly that. The connector arrives labelled as well, so
/// the holder can pass it on and the child's own manifest row still decides
/// everything else it gets.
pub const PROVIDE_PREFIX: &str = "provide:";

/// Endowed `(label, handle)` pairs one spawn may carry. Policy on the
/// primitive, refused by name, never truncated — the widest manifest row plus
/// stdio.
pub const MAX_ENDOWMENTS: usize = 32;
/// `(child slot, parent handle)` pairs one spawn may carry.
///
/// **Derived rather than chosen.** A slot map installs into the child's table,
/// which has [`RawHandle::MAX_SLOTS`] slots, so a longer one necessarily names
/// a slot twice and the second pair says everything the first did. Without it
/// the count was bounded only by the 2 MiB window the arguments are read
/// through — 262,144 pairs, every one of them a `duplicate_entry` under the
/// parent's own lock, and every repeat of one slot displacing a live entry the
/// caller has to carry out of that lock. That vector passes `MAX_HEAP_ALLOC`
/// at 87,211 repeats, and the allocator's refusal there is a kernel panic.
pub const MAX_SLOT_MAP: usize = RawHandle::MAX_SLOTS;
/// Bytes of label blob one endowment table may carry.
pub const MAX_LABELS_LEN: usize = 4096;

use crate::handle::Rights;
use crate::{Pid, RawHandle, HANDLE_INVALID};

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
    /// The device did not do it.
    ///
    /// One word for a transfer that was issued and not completed, and for a
    /// volume whose own structures do not decode: there is nothing a caller can
    /// do differently about the two, and both are the opposite of `NotFound`.
    /// The channel below it — `block::BlockDevice`, `FileBacking::read_page`,
    /// `bcachefs::BlockIO`, `vfs::FileSystem` — is fallible the whole way so
    /// that this is what arrives, rather than "no such file".
    ///
    /// It carries nothing. Which endpoint stalled, what the sense key was and
    /// which block was asked for are in the kernel's own log line, where a
    /// triage reads them; an enum here would be a vocabulary userland has no
    /// use for and every new driver would have to guess an arm from.
    Io = 9,
    /// The object was there and its other end is not.
    ///
    /// **A different fact from `NotFound`, and the design does not work without
    /// the difference.** "The name is not in the namespace this process was
    /// given" is a statement about this process and the answer is "you have a
    /// bug"; "the server exited" is a statement about the machine. The SDK sees
    /// one `u64`, so if the kernel gives one word the SDK has one answer — and
    /// the same rule the storage layer already obeys applies here: a dead peer
    /// must not be indistinguishable from a handle that was never there.
    Gone = 10,
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
            9 => Some(Self::Io),
            10 => Some(Self::Gone),
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
            Self::Io => f.write_str("the device did not complete the transfer"),
            Self::Gone => f.write_str("the other end is gone"),
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
///
/// The kernel maps 2 MiB pages and has no `mprotect`, so what a mapping is
/// created with is what it stays. `NONE` reserves the address range and maps
/// nothing at all: any access to it faults, which is the guard page a libc
/// asks for. Anything without `WRITE` is mapped read-only, and a store to it
/// is a protection violation that kills the process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MmapProt(pub u64);

impl MmapProt {
    pub const NONE: Self = Self(0);
    pub const READ: Self = Self(1);
    pub const WRITE: Self = Self(2);

    pub const fn contains(self, flag: Self) -> bool { self.0 & flag.0 != 0 }
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

    pub const fn contains(self, flag: Self) -> bool { self.0 & flag.0 != 0 }
}

impl core::ops::BitOr for MmapFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self { Self(self.0 | rhs.0) }
}

/// Result of [`pipe`]: the read and write ends.
#[derive(Debug, Clone, Copy)]
pub struct PipeFds {
    pub read: RawHandle,
    pub write: RawHandle,
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

/// Write bytes to a file descriptor. Returns number of bytes written.
pub fn write(fd: RawHandle, buf: &[u8]) -> Result<usize, SyscallError> {
    check(syscall(SYS_WRITE, fd.0 as u64, buf.as_ptr() as u64, buf.len() as u64, 0)).map(|n| n as usize)
}

/// Read bytes from a file descriptor. Returns number of bytes read.
pub fn read(fd: RawHandle, buf: &mut [u8]) -> Result<usize, SyscallError> {
    check(syscall(SYS_READ, fd.0 as u64, buf.as_mut_ptr() as u64, buf.len() as u64, 0)).map(|n| n as usize)
}

/// Exit the current thread only. Does not return.
/// Use `exit()` to exit the entire process (all threads).
pub fn thread_exit(code: i32) -> ! {
    loop { syscall(SYS_THREAD_EXIT, code as u64, 0, 0, 0); }
}

/// Exit the entire process (all threads) with `code`. Does not return.
pub fn exit(code: i32) -> ! {
    loop { syscall(SYS_EXIT, code as u64, 0, 0, 0); }
}

/// Ask the kernel to do one of the things [`SYS_DEBUG`] does. The actions are
/// the `DEBUG_*` constants below, not bare numbers.
pub fn debug(action: u64) -> u64 {
    syscall(SYS_DEBUG, action, 0, 0, 0)
}

/// [`debug`] for the actions that take an argument.
pub fn debug_with(action: u64, arg: u64) -> u64 {
    syscall(SYS_DEBUG, action, arg, 0, 0)
}

/// What [`SYS_DEBUG`] can be asked to do.
///
/// **One declaration, because a number that means something is a constant.**
/// These were bare integers in the kernel's dispatch and re-declared in each
/// test binary that called one — five spellings of `14`, and the kernel's own
/// arms said nothing about which was which except in prose beside them. An
/// action's *reason* stays at the kernel arm, where the code it runs is; its
/// number lives here once, where both sides read it.
///
/// **Every action here needs a kernel built with `test-actuators`, and a kernel
/// without it has no `SYS_DEBUG` at all** — the number falls to the dispatch's
/// default and answers `InvalidArgument`, which is what an unassigned number
/// answers. So a constant below is a number a shipping kernel does not have,
/// and the caller of one is a test that has to be given the kernel that does.
pub mod debug_action {
    /// Panic the kernel. Kills the caller's machine, deliberately.
    pub const PANIC: u64 = 0;
    /// Read through a null pointer in Ring 0.
    pub const NULL_READ: u64 = 1;
    /// Hold a kernel lock across a scheduler entry. Armed once per boot.
    pub const LOCK_ACROSS_SWITCH: u64 = 2;
    /// Halt every CPU.
    pub const FATAL_HALT: u64 = 3;
    /// Provoke a real #DF, to exercise the IST1 stack.
    pub const DOUBLE_FAULT: u64 = 4;
    /// Heap allocations either side of `MAX_HEAP_ALLOC`.
    pub const HEAP_AT_CEILING: u64 = 5;
    pub const HEAP_OVER_CEILING: u64 = 6;
    pub const HEAP_AT_CEILING_PAGE_ALIGNED: u64 = 7;
    /// Draw over the screen a userland process owns.
    pub const SCREEN_GRAFFITI: u64 = 8;
    /// Read the guard page below this CPU's idle stack.
    pub const IDLE_GUARD_READ: u64 = 9;
    /// The kernel canary's address, and whether it still holds what the kernel
    /// wrote.
    pub const CANARY_ADDR: u64 = 10;
    pub const CANARY_CHANGED: u64 = 11;
    /// Make the last CPU a shootdown waits for answer `arg` nanoseconds late,
    /// and take it away again.
    pub const TLB_ACK_DELAY_ARM: u64 = 12;
    pub const TLB_ACK_DELAY_DISARM: u64 = 13;
    /// How many kernel objects are alive right now, machine-wide.
    pub const CENSUS_TOTAL: u64 = 14;
    /// The same, per kind, into the kernel log.
    pub const CENSUS_BREAKDOWN: u64 = 15;
    /// The same for one [`OBJECT_KINDS`](super::OBJECT_KINDS) index, which is
    /// the argument. The one a guest test can assert on.
    pub const CENSUS_KIND: u64 = 16;
    /// The deepest any CPU's idle stack has been this boot, in bytes.
    ///
    /// The idle loop is where `object::drain_zero_handles` releases objects
    /// with nothing held, and a release path that reaches the filesystem is the
    /// deepest thing this kernel does. This is how a test asserts that stack is
    /// sized for it, rather than waiting for the guard page below it to say so
    /// by halting the machine.
    pub const IDLE_STACK_HIGH_WATER: u64 = 17;
    /// How big that stack is, so the reading above is a *fraction* rather than
    /// a number nobody can judge. The size is the kernel's choice and not the
    /// ABI's, which is why it is asked for rather than declared here.
    pub const IDLE_STACK_SIZE: u64 = 18;
    /// Put a count this guest can reach in `MAX_SYSINFO_THREADS`'s place, for
    /// the rest of the boot. `SYS_SYSINFO`'s real bound is a thread count no
    /// guest can make, so only the number can move and moving it runs the
    /// shipped count, comparison and refusal.
    pub const LOWER_SYSINFO_BOUND: u64 = 19;
}

/// Every kind of kernel object, in the order the kernel's own `kobject!`
/// declares them — so an index into this is the index
/// [`debug_action::CENSUS_KIND`] takes.
///
/// **In the ABI rather than in the kernel alone, because a census nobody can
/// read per kind is a total.** Every leak assertion in the test estate was
/// against the machine-wide count, where a leak of one kind is hidden by churn
/// in another, and six of the thirteen kinds were exercised by no census
/// assertion at all. The kernel checks this list against its own declaration
/// order when the action is called, so a row added to `kobject!` without a row
/// here is a named refusal rather than an index that quietly names its
/// neighbour.
pub const OBJECT_KINDS: &[&str] = &[
    "PipeRead",
    "PipeWrite",
    "Connection",
    "Device",
    "Acceptor",
    "IoUring",
    "SharedMem",
    "Connector",
    "Namespace",
    "File",
    "Console",
    "SysCap",
    "Process",
];

/// Create a pipe. Returns the read and write ends.
///
/// **Fallible, because `sys_pipe` is.** It answers `ResourceExhausted` on three
/// paths — no pipe pages, and either handle install hitting the table cap — and
/// a wrapper that cannot say so hands the caller a pair of handles that are an
/// error word cut in half.
///
/// A packed pair can never be mistaken for an error word: no handle is ever
/// `0xFFFF_FFFF`, because a slot at `MAX_GENERATION` is retired rather than
/// reissued, and `SyscallError` occupies only the top 256 values.
pub fn pipe() -> Result<PipeFds, SyscallError> {
    let raw = check(syscall(SYS_PIPE, 0, 0, 0, 0))?;
    Ok(PipeFds {
        read: RawHandle((raw >> 32) as u32),
        write: RawHandle((raw & 0xFFFF_FFFF) as u32),
    })
}

/// Read the inherited environment variables into `buf`.
/// Returns the number of bytes written, or the required size if buf is too small.
pub fn get_env(buf: &mut [u8]) -> usize {
    syscall(SYS_GET_ENV, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0) as usize
}

/// Spawn a new process. The `SpawnArgs` struct contains argv, fd_map, and env.
///
/// Answers a `Process` handle carrying `WAIT|MANAGE|READ|DUP|TRANSFER`. A
/// caller that wants nothing to do with the child closes it; a caller that
/// wants to hand it on transfers it. There is no pid-addressed way back to a
/// process, so this handle is the whole of what a spawn confers.
///
/// # Safety
/// The raw pointer fields in `SpawnArgs` must point to valid memory.
pub unsafe fn spawn(args: &SpawnArgs) -> Result<RawHandle, SyscallError> {
    check(syscall(SYS_SPAWN, args as *const SpawnArgs as u64, 0, 0, 0))
        .map(|h| RawHandle(h as u32))
}

/// Read this process's endowment table into `buf`: an `[EndowEntry]` count and
/// entries followed by the label blob. Returns the bytes written, or — when
/// `buf` is empty — the bytes the answer needs.
///
/// The one place a name is resolved to a handle at all: there is no global
/// registry, so a process learns what it holds only from its own table.
pub fn endowments(buf: &mut [u8]) -> usize {
    syscall(SYS_ENDOWMENTS, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0) as usize
}

/// Block until the process `proc` names has exited, and take its exit code.
pub fn process_wait(proc: RawHandle) -> Result<i32, SyscallError> {
    check(syscall(SYS_PROCESS_WAIT, proc.0 as u64, 0, 0, 0)).map(|code| code as i32)
}

/// The exit code if the process has already exited, `Err(WouldBlock)` if it has
/// not.
///
/// [`WNOHANG`] rather than a syscall of its own: this is the same question with
/// the wait taken out, and a caller that polls is asking about the same object.
pub fn process_wait_nonblock(proc: RawHandle) -> Result<i32, SyscallError> {
    check(syscall(SYS_PROCESS_WAIT, proc.0 as u64, WNOHANG, 0, 0)).map(|code| code as i32)
}

/// Kill the process `proc` names. Answers `Ok` for one already dead: the
/// caller asked for it to be gone and it is.
pub fn process_kill(proc: RawHandle) -> Result<(), SyscallError> {
    check_unit(syscall(SYS_PROCESS_KILL, proc.0 as u64, 0, 0, 0))
}

/// A `Process` handle for `pid`, presenting a `SysCap` that carries
/// `Rights::MANAGE`.
pub fn process_open(syscap: RawHandle, pid: Pid) -> Result<RawHandle, SyscallError> {
    check(syscall(SYS_PROCESS_OPEN, syscap.0 as u64, pid.0 as u64, 0, 0))
        .map(|h| RawHandle(h as u32))
}

/// Copy records into `out`, oldest first and merged by `at_ns`, advancing
/// `cursor`. Answers how many records were written, and `0` when there is
/// nothing new.
///
/// **It never blocks.** A caller with nothing to read arms on a readiness
/// source and parks; a syscall that waited would be a second blocking mechanism
/// in a kernel that is converging on one.
///
/// `out` is whole [`crate::log::LogRecord`]s at a fixed stride, so the caller
/// indexes by shift and the kernel does no length arithmetic. A buffer that
/// cannot hold one record, or that cannot hold what the machine's shard count
/// requires, is `InvalidArgument` — untrusted input that cannot be satisfied is
/// refused, never truncated to fit.
pub fn log_read(
    syscap: RawHandle,
    cursor: &mut crate::log::LogCursor,
    out: &mut [crate::log::LogRecord],
) -> Result<usize, SyscallError> {
    check(syscall(
        SYS_LOG_READ,
        syscap.0 as u64,
        cursor as *mut crate::log::LogCursor as u64,
        out.as_mut_ptr() as u64,
        out.len() as u64,
    ))
    .map(|n| n as usize)
}

/// Mark file descriptor as the controlling TTY for this process.
pub fn mark_tty(fd: RawHandle) {
    syscall(SYS_MARK_TTY, fd.0 as u64, 0, 0, 0);
}

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

/// Open a file.
pub fn open(path: &[u8], flags: OpenFlags) -> Result<RawHandle, SyscallError> {
    check(syscall(SYS_OPEN, path.as_ptr() as u64, path.len() as u64, flags.0, 0)).map(|v| RawHandle(v as u32))
}

/// Close a file descriptor.
pub fn close(fd: RawHandle) {
    syscall(SYS_CLOSE, fd.0 as u64, 0, 0, 0);
}

/// Seek within a file descriptor. Returns new offset.
pub fn seek(fd: RawHandle, pos: SeekFrom) -> Result<u64, SyscallError> {
    let (offset, whence) = match pos {
        SeekFrom::Start(n) => (n as i64, 0u64),
        SeekFrom::Current(n) => (n, 1u64),
        SeekFrom::End(n) => (n, 2u64),
    };
    check(syscall(SYS_SEEK, fd.0 as u64, offset as u64, whence, 0))
}

/// Get file metadata for a file descriptor.
pub fn fstat(fd: RawHandle) -> Result<Stat, SyscallError> {
    let mut stat = Stat { file_type: FileType::Unknown, size: 0, mtime: 0 };
    check_unit(syscall(SYS_FSTAT, fd.0 as u64, &mut stat as *mut Stat as u64, 0, 0))?;
    Ok(stat)
}

/// Flush file descriptor to disk.
pub fn fsync(fd: RawHandle) -> Result<(), SyscallError> {
    check_unit(syscall(SYS_FSYNC, fd.0 as u64, 0, 0, 0))
}

/// Read directory entries. Returns the number of bytes the listing *needs*.
///
/// `Ok(n)` with `n <= buf.len()` means the entries are in `buf`; `n >
/// buf.len()` means nothing was written and `n` is the size to retry with.
/// The kernel never writes a partial listing — see `sys_readdir`.
///
/// The error is returned rather than folded into `0`, which is what this did
/// before: "the directory is too large to list" and "the directory is empty"
/// are different answers and a caller has to be able to tell them apart.
pub fn readdir(path: &[u8], buf: &mut [u8]) -> Result<usize, SyscallError> {
    let n = syscall(SYS_READDIR, path.as_ptr() as u64, path.len() as u64, buf.as_mut_ptr() as u64, buf.len() as u64);
    match SyscallError::from_u64(n) {
        Some(e) => Err(e),
        None => Ok(n as usize),
    }
}

/// Delete a file or directory.
pub fn delete(path: &[u8]) -> Result<(), SyscallError> {
    check_unit(syscall(SYS_DELETE, path.as_ptr() as u64, path.len() as u64, 0, 0))
}

/// Change current working directory.
pub fn chdir(path: &[u8]) -> Result<(), SyscallError> {
    check_unit(syscall(SYS_CHDIR, path.as_ptr() as u64, path.len() as u64, 0, 0))
}

/// Get the current working directory.
///
/// Returns the length the path *needs*, not the number of bytes written:
/// `n <= buf.len()` means the path is in `buf[..n]`, and `n > buf.len()` means
/// nothing was written and `n` is the size to allocate before retrying. Pass an
/// empty buffer to ask the length alone. `0` is the error return.
///
/// Reporting the required length rather than a truncated count is what lets a
/// caller be correct: the previous contract could not distinguish an exact fit
/// from a silent truncation, so a caller with a fixed buffer got a valid-looking
/// path to the wrong directory.
pub fn getcwd(buf: &mut [u8]) -> usize {
    let n = syscall(SYS_GETCWD, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0);
    if SyscallError::from_u64(n).is_some() { 0 } else { n as usize }
}

/// Fill `buf` with cryptographically secure random bytes.
pub fn random(buf: &mut [u8]) {
    syscall(SYS_RANDOM, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0);
}

/// Nanoseconds since boot (monotonic clock).
pub fn clock_nanos() -> u64 {
    syscall(SYS_CLOCK, 0, 0, 0, 0)
}

/// The time of day in the zone the machine keeps its clock in.
///
/// `None` is a machine that never said what time it is — an RTC that is absent,
/// wedged, or answering with something that is not a date. It is `None` for the
/// whole of such a boot rather than intermittently, because the kernel reads
/// the clock once.
pub fn clock_realtime() -> Option<RealTime> {
    let raw = check(syscall(SYS_CLOCK_REALTIME, 0, 0, 0, 0)).ok()?;
    Some(RealTime {
        hours: ((raw >> 16) & 0xFF) as u8,
        minutes: ((raw >> 8) & 0xFF) as u8,
        seconds: (raw & 0xFF) as u8,
    })
}

/// Seconds since the Unix epoch (1970-01-01 00:00:00 UTC).
///
/// `None` on the same machine and for the same reason as [`clock_realtime`].
/// Cheap: the kernel serves it from an anchor it took at boot plus the
/// monotonic clock, so this is a syscall and not a device access.
pub fn clock_epoch() -> Option<u64> {
    check(syscall(SYS_CLOCK_EPOCH, 0, 0, 0, 0)).ok()
}

/// Two `u32`s in one argument word.
///
/// The four device calls below take the claim handle that authorizes them, and
/// `SYS_GPU_PRESENT`'s rectangle then does not fit in what is left. A pair is a
/// wire encoding decoded at the kernel boundary and carried no further.
const fn pair(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

/// Transfer a region of the framebuffer to the GPU and flush it, presenting the
/// framebuffer claim. Pass (0, 0, 0, 0) to flush the full screen.
///
/// Fallible: the kernel refuses a handle that is not a live framebuffer claim.
pub fn gpu_present(claim: RawHandle, x: u32, y: u32, w: u32, h: u32) -> Result<(), SyscallError> {
    check_unit(syscall(SYS_GPU_PRESENT, claim.0 as u64, pair(x, y), pair(w, h), 0))
}

/// Upload the cursor image from backing and enable hardware cursor.
pub fn gpu_set_cursor(claim: RawHandle, hot_x: u32, hot_y: u32) -> Result<(), SyscallError> {
    check_unit(syscall(SYS_GPU_SET_CURSOR, claim.0 as u64, hot_x as u64, hot_y as u64, 0))
}

/// Move the hardware cursor to screen position (x, y).
pub fn gpu_move_cursor(claim: RawHandle, x: u32, y: u32) -> Result<(), SyscallError> {
    check_unit(syscall(SYS_GPU_MOVE_CURSOR, claim.0 as u64, x as u64, y as u64, 0))
}

/// Request a GPU resolution change. On success, writes the new
/// [`FramebufferInfo`](crate::FramebufferInfo) to `info_out`.
///
/// # Safety
/// `info_out` must point to a writable buffer of at least
/// `size_of::<FramebufferInfo>()` bytes.
pub unsafe fn gpu_set_resolution(
    claim: RawHandle,
    width: u32,
    height: u32,
    info_out: *mut u8,
) -> Result<(), SyscallError> {
    check_unit(syscall(
        SYS_GPU_SET_RESOLUTION,
        claim.0 as u64,
        pair(width, height),
        info_out as u64,
        0,
    ))
}

/// Shut down the machine. Does not return.
pub fn shutdown() -> ! {
    syscall(SYS_SHUTDOWN, 0, 0, 0, 0);
    loop {}
}

/// The device classes, their wire numbers, and the name a `system.toml`
/// `devices` entry and a `dev:` endowment label spell each one with.
///
/// **One row per class, so the four cannot disagree.** The build system checks
/// a config against this table, `/bin/init` mints from it, and a claimant finds
/// its own claim by it; a second spelling anywhere is a class a config can name
/// and no program can find. The wire number is here too, because a class whose
/// number and name came from different lists is the same defect one level down.
macro_rules! device_classes {
    ($($(#[$meta:meta])* $variant:ident = $num:literal => $name:literal),+ $(,)?) => {
        #[repr(u64)]
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum DeviceType {
            $($(#[$meta])* $variant = $num),+
        }

        impl DeviceType {
            /// What a manifest calls this class.
            pub fn class_name(self) -> &'static str {
                match self { $(Self::$variant => $name),+ }
            }

            /// The class a config named, or `None` — a typo in a `devices`
            /// list, refused where the image is built.
            pub fn from_class_name(name: &str) -> Option<Self> {
                match name { $($name => Some(Self::$variant),)+ _ => None }
            }

            /// The wire number a syscall carries, decoded once.
            pub fn from_raw(raw: u64) -> Option<Self> {
                match raw { $($num => Some(Self::$variant),)+ _ => None }
            }

            /// Every class, for a caller that must consider all of them.
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];
        }
    };
}

device_classes! {
    Keyboard = 0 => "keyboard",
    Mouse = 1 => "mouse",
    Framebuffer = 2 => "framebuffer",
    Nic = 3 => "nic",
    // 4 was `Audio`, a sound card the kernel drove on the claimant's behalf.
    // Retired with the syscall that fed it rather than reused for the stub that
    // replaced it: the claim authorizes register writes and answers no submit,
    // so a caller that still names 4 has to be refused rather than handed a
    // capability of a different shape.
    /// An Intel HDA controller the kernel has brought up but drives no policy
    /// on.
    HdaAudio = 5 => "hda-audio",
    /// A virtio-sound device, on the same terms: the kernel negotiated its
    /// features, built its virtqueues and owns their descriptors, and every
    /// decision above that — the stream, the rate, the format, when a period is
    /// published — belongs to whoever holds this.
    VirtioSound = 6 => "virtio-sound",
}

/// Mint a device claim for `class`, presenting a `SysCap` handle that carries
/// [`Rights::DEVICE`]. `NotFound` for a class no driver registered — init
/// endows what exists and logs what it did not.
///
/// The claim comes back **without** [`Rights::DUP`], so it can only be moved,
/// which is what makes endowing one to a child a provable hand-off.
///
/// [`Rights::DEVICE`]: crate::handle::Rights::DEVICE
/// [`Rights::DUP`]: crate::handle::Rights::DUP
pub fn device_claim(syscap: RawHandle, class: DeviceType) -> Result<RawHandle, SyscallError> {
    check(syscall(SYS_DEVICE_CLAIM, syscap.0 as u64, class as u64, 0, 0))
        .map(|v| RawHandle(v as u32))
}

/// Enter the real-time scheduling band, presenting a `SysCap` handle that
/// carries [`Rights::RT`]. The privilege a device claim was never enough to
/// confer.
///
/// [`Rights::RT`]: crate::handle::Rights::RT
pub fn rt_enter(syscap: RawHandle) -> Result<(), SyscallError> {
    check_unit(syscall(SYS_RT_ENTER, syscap.0 as u64, 0, 0, 0))
}

/// How wide a register access is.
///
/// Not a convenience: a device's registers are 8, 16 and 32 bits and a 32-bit
/// write to a 16-bit register is a write to its neighbour — HDA's `SDnCTL` and
/// `SDnSTS` are adjacent bytes of one dword, and the second is the kernel's
/// alone.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u64)]
pub enum RegWidth {
    U8 = 1,
    U16 = 2,
    U32 = 4,
}

impl RegWidth {
    pub fn from_raw(raw: u64) -> Option<Self> {
        match raw {
            1 => Some(Self::U8),
            2 => Some(Self::U16),
            4 => Some(Self::U32),
            _ => None,
        }
    }

    pub const fn bytes(self) -> u64 {
        self as u64
    }

    /// The widest value this access can carry. A caller handing a wider one is
    /// naming bits the register does not have.
    pub const fn max_value(self) -> u32 {
        match self {
            Self::U8 => u8::MAX as u32,
            Self::U16 => u16::MAX as u32,
            Self::U32 => u32::MAX,
        }
    }
}

/// Read one register of the device `fd` claims.
///
/// `offset` is a byte offset inside that device's register window. The kernel
/// checks it against the device's read allow-list and refuses anything else by
/// name; there is no way to name an address here and no way to reach a
/// register the list does not carry.
pub fn device_reg_read(
    fd: RawHandle,
    offset: u32,
    width: RegWidth,
) -> Result<u32, SyscallError> {
    check(syscall(SYS_DEVICE_REG_READ, fd.0 as u64, offset as u64, width.bytes(), 0))
        .map(|v| v as u32)
}

/// Write one register of the device `fd` claims.
///
/// The allow-list is positive and per-device: an entry is on it because its
/// value is not an address and indexes nothing the kernel allocated. A missing
/// entry costs a driver that cannot bring its stream up and says so, which is
/// the failure mode a refusal list does not have.
pub fn device_reg_write(
    fd: RawHandle,
    offset: u32,
    width: RegWidth,
    value: u32,
) -> Result<(), SyscallError> {
    check(syscall(
        SYS_DEVICE_REG_WRITE,
        fd.0 as u64,
        offset as u64,
        width.bytes(),
        value as u64,
    ))
    .map(|_| ())
}

// Ports and namespaces

/// Both ends of a fresh port.
///
/// Two types and not one object with a direction right: "accept the
/// connections of a service you were only given access to" is a state that
/// cannot be written, the same way a pipe's two ends are two types.
pub struct Port {
    pub acceptor: RawHandle,
    pub connector: RawHandle,
}

/// Make a port. Needs no right and grants none — a port with no clients is not
/// authority.
pub fn port_create() -> Result<Port, SyscallError> {
    let raw = syscall(SYS_PORT_CREATE, 0, 0, 0, 0);
    if let Some(e) = SyscallError::from_u64(raw) {
        return Err(e);
    }
    Ok(Port {
        acceptor: RawHandle((raw >> 32) as u32),
        connector: RawHandle((raw & 0xFFFF_FFFF) as u32),
    })
}

/// One `(name, connector)` pair `SYS_NAMESPACE_BUILD` adds.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NamespaceEntry {
    pub off: u32,
    pub len: u32,
    pub connector: RawHandle,
    /// Named, so nothing leaks kernel stack into it.
    pub _pad: u32,
}

/// One name carried over from the base namespace.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NameRef {
    pub off: u32,
    pub len: u32,
}

/// Arguments for [`SYS_NAMESPACE_BUILD`], passed as a single pointer.
///
/// A namespace is immutable once built: there is no insert, no remove and no
/// replace, so a narrower one is a *new* object built from this one and a
/// handle to a namespace is a handle to a fixed set.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NamespaceBuild {
    /// [`HANDLE_INVALID`] for an empty base.
    ///
    /// [`HANDLE_INVALID`]: crate::handle::HANDLE_INVALID
    pub base: RawHandle,
    pub _pad: u32,
    /// `[NameRef]` — the names to carry over from `base`.
    pub keep_ptr: u64,
    pub keep_n: u64,
    /// `[NamespaceEntry]` — new bindings.
    pub add_ptr: u64,
    pub add_n: u64,
    /// The blob every `off`/`len` above indexes into.
    pub names_ptr: u64,
    pub names_len: u64,
}

const _: () = assert!(core::mem::size_of::<NamespaceBuild>() == 56);
const _: () = assert!(core::mem::size_of::<NamespaceEntry>() == 16);
const _: () = assert!(core::mem::size_of::<NameRef>() == 8);

/// Names one namespace may bind. Policy on the primitive; a caller asking for
/// one more is refused by name and never truncated.
pub const MAX_NAMESPACE_ENTRIES: usize = 64;
/// Bytes in one service name.
pub const MAX_SERVICE_NAME: usize = 64;

/// # Safety
/// Every pointer in `args` must name `args`'s stated length of readable memory.
pub unsafe fn namespace_build(args: &NamespaceBuild) -> Result<RawHandle, SyscallError> {
    check(syscall(SYS_NAMESPACE_BUILD, args as *const _ as u64, 0, 0, 0))
        .map(|v| RawHandle(v as u32))
}

/// Open a connection to `name` in the namespace `ns` holds.
///
/// `NotFound` means the name is not in this namespace — a fact about this
/// process. [`SyscallError::Gone`] means the server that held the acceptor has
/// exited. There is no third answer, and in particular there is no "not yet":
/// the port exists before either process runs.
pub fn namespace_open(ns: RawHandle, name: &str) -> Result<RawHandle, SyscallError> {
    check(syscall(
        SYS_NAMESPACE_OPEN,
        ns.0 as u64,
        name.as_ptr() as u64,
        name.len() as u64,
        0,
    ))
    .map(|v| RawHandle(v as u32))
}

/// Accept a queued connection. Blocks until there is one.
///
/// **It answers with the connection and nothing else.** Who connected is not
/// the kernel's to assert: a server that wants to name its client reads it out
/// of the protocol's first frame, where it is already the client's own claim
/// about itself and already distrusted.
pub fn accept(acceptor: RawHandle) -> Result<RawHandle, SyscallError> {
    check(syscall(SYS_ACCEPT, acceptor.0 as u64, 0, 0, 0)).map(|v| RawHandle(v as u32))
}

/// Join a pipe read end and a pipe write end into one duplex connection.
///
/// The two ends stay open and this takes references of its own, so the caller
/// closes what it handed in. The result carries no handle-transfer queue: a
/// connection made this way has no peer holding the other half, so
/// [`handle_send`] on one answers [`SyscallError::Gone`].
pub fn connection_join(rx: RawHandle, tx: RawHandle) -> Result<RawHandle, SyscallError> {
    check(syscall(SYS_CONNECTION_JOIN, rx.0 as u64, tx.0 as u64, 0, 0))
        .map(|v| RawHandle(v as u32))
}

/// Handles one [`handle_send`] may carry.
///
/// Policy on the primitive: a caller asking for one more is refused by name
/// and never truncated to fit.
pub const MAX_TRANSFER_HANDLES: usize = 8;

/// Batches one direction of a connection may hold unreceived.
pub const MAX_QUEUED_BATCHES: usize = 16;

/// Move `handles` to the peer of `conn`.
///
/// Each handle must carry [`Rights::TRANSFER`], and so must `conn`. The move is
/// all-or-nothing: a refusal leaves every handle where it was.
///
/// **Send the handles before the frame that announces them.** They travel in a
/// queue of their own rather than interleaved with the connection's bytes, so a
/// peer that has read the frame is guaranteed to find them only in that order.
///
/// [`Rights::TRANSFER`]: crate::handle::Rights::TRANSFER
pub fn handle_send(conn: RawHandle, handles: &[RawHandle]) -> Result<(), SyscallError> {
    check_unit(syscall(
        SYS_HANDLE_SEND,
        conn.0 as u64,
        handles.as_ptr() as u64,
        handles.len() as u64,
        0,
    ))
}

/// Take the oldest batch the peer sent, into `out`. Answers how many it wrote.
///
/// Never blocks: zero means nothing is queued right now. A batch larger than
/// `out` is `InvalidArgument` and stays queued — `out` should be
/// [`MAX_TRANSFER_HANDLES`] long.
pub fn handle_recv(conn: RawHandle, out: &mut [RawHandle]) -> Result<usize, SyscallError> {
    check(syscall(
        SYS_HANDLE_RECV,
        conn.0 as u64,
        out.as_mut_ptr() as u64,
        out.len() as u64,
        0,
    ))
    .map(|n| n as usize)
}

/// Make a shared-memory region and answer a handle to it.
///
/// Fallible: a size the kernel cannot express in whole 2 MiB pages is
/// `InvalidArgument` and memory it does not have is `ResourceExhausted`. A
/// daemon reaches both through a client's request, so neither may be an
/// assertion here.
pub fn shm_create(size: usize) -> Result<RawHandle, SyscallError> {
    check(syscall(SYS_SHM_CREATE, size as u64, 0, 0, 0)).map(|v| RawHandle(v as u32))
}

/// Map the region `shm` names into this process. Needs [`Rights::MAP`].
///
/// Idempotent: a second call answers the first call's address.
///
/// [`Rights::MAP`]: crate::handle::Rights::MAP
///
/// # Safety
/// Caller must manage the returned pointer.
pub unsafe fn shm_map(shm: RawHandle) -> Result<*mut u8, SyscallError> {
    check(syscall(SYS_SHM_MAP, shm.0 as u64, 0, 0, 0))
        .map(|addr| core::ptr::with_exposed_provenance_mut(addr as usize))
}

/// Unmap the region `shm` names from this process. Needs [`Rights::MAP`].
///
/// [`Rights::MAP`]: crate::handle::Rights::MAP
pub fn shm_unmap(shm: RawHandle) -> Result<(), SyscallError> {
    check_unit(syscall(SYS_SHM_UNMAP, shm.0 as u64, 0, 0, 0))
}

/// Query system information (memory, CPUs, processes).
/// Returns the number of bytes written to `buf`.
pub fn sysinfo(buf: &mut [u8]) -> usize {
    let n = syscall(SYS_SYSINFO, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0);
    if SyscallError::from_u64(n).is_some() { 0 } else { n as usize }
}

/// Sleep for the given number of nanoseconds.
pub fn nanosleep(nanos: u64) {
    syscall(SYS_NANOSLEEP, nanos, 0, 0, 0);
}

/// What [`SYS_HANDLE_DUP`]'s rights word carries when the caller wants the
/// source's own set.
///
/// A wire encoding of `Option<Rights>`, decoded at the syscall boundary and
/// never carried inward: `Rights` is nine bits, so this value is not one. The
/// two wrappers below are the only writers, so no caller ever spells it.
pub const RIGHTS_UNCHANGED: u64 = u64::MAX;

/// A second handle to the same object, carrying what the first carries.
pub fn dup(handle: RawHandle) -> Result<RawHandle, SyscallError> {
    check(syscall(SYS_HANDLE_DUP, handle.0 as u64, RIGHTS_UNCHANGED, 0, 0))
        .map(|v| RawHandle(v as u32))
}

/// A second handle to the same object, carrying **less**.
///
/// `PermissionDenied` for a set the source does not itself hold: rights only
/// shrink, and asking to widen is a bug in the asker rather than a request to
/// be quietly cut down to size. This is how init hands a program an `RT`-only
/// `SysCap` while keeping the full one.
pub fn dup_narrowed(handle: RawHandle, rights: Rights) -> Result<RawHandle, SyscallError> {
    check(syscall(SYS_HANDLE_DUP, handle.0 as u64, rights.bits() as u64, 0, 0))
        .map(|v| RawHandle(v as u32))
}

/// A second handle to the same object, at a **slot** the caller picks.
///
/// A slot and not a handle: a handle carries a generation the caller has no
/// business choosing, and the one this hands back is the slot's own — so the
/// answer is not the number that went in. Whatever was at that slot is closed
/// first.
/// The rights are the source's: narrowing at a slot is [`dup_narrowed`]
/// followed by this, and a third argument no caller writes would be a right
/// nobody can request.
pub fn dup2(handle: RawHandle, slot: u16) -> Result<RawHandle, SyscallError> {
    check(syscall(SYS_HANDLE_DUP_AT, handle.0 as u64, slot as u64, 0, 0))
        .map(|v| RawHandle(v as u32))
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

/// Truncate file descriptor to `size` bytes.
pub fn ftruncate(fd: RawHandle, size: u64) -> Result<(), SyscallError> {
    check_unit(syscall(SYS_FTRUNCATE, fd.0 as u64, size, 0, 0))
}

/// Get the current thread's stack base address and size.
pub fn stack_info() -> Option<(u64, u64)> {
    let mut base: u64 = 0;
    let mut size: u64 = 0;
    let r = syscall(SYS_STACK_INFO, &mut base as *mut u64 as u64, &mut size as *mut u64 as u64, 0, 0);
    if SyscallError::from_u64(r).is_some() { None } else { Some((base, size)) }
}

/// Return the number of available CPUs.
pub fn cpu_count() -> u32 {
    syscall(SYS_CPU_COUNT, 0, 0, 0, 0) as u32
}

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

/// Non-blocking read. Returns bytes read, or `Err(WouldBlock)` if no data available.
pub fn read_nonblock(fd: RawHandle, buf: &mut [u8]) -> Result<usize, SyscallError> {
    check(syscall(SYS_READ_NONBLOCK, fd.0 as u64, buf.as_mut_ptr() as u64, buf.len() as u64, 0)).map(|n| n as usize)
}

/// Non-blocking write. Returns bytes written, or `Err(WouldBlock)` if no space available.
pub fn write_nonblock(fd: RawHandle, buf: &[u8]) -> Result<usize, SyscallError> {
    check(syscall(SYS_WRITE_NONBLOCK, fd.0 as u64, buf.as_ptr() as u64, buf.len() as u64, 0)).map(|n| n as usize)
}

/// Map a pipe's shared-memory ring buffer into this process's address space.
/// Returns a pointer to the `RingHeader` at the start of the mapped region.
///
/// The mapping is writable, and the header is a publication: writing it tells
/// the kernel nothing. Reads and writes still go through `SYS_READ`/`SYS_WRITE`.
pub fn pipe_map(fd: RawHandle) -> Result<*mut u8, SyscallError> {
    check(syscall(SYS_PIPE_MAP, fd.0 as u64, 0, 0, 0)).map(|v| v as *mut u8)
}

/// Poll for a received frame, presenting the NIC claim. Returns
/// `(buf_index << 16) | frame_len`, or 0 if none.
///
/// Fallible: the kernel refuses a handle that is not a live NIC claim. The
/// packed success value tops out at `(255 << 16) | 4096`, far below the range
/// `SyscallError::from_u64` claims, so nothing is ambiguous.
pub fn nic_rx_poll(claim: RawHandle) -> Result<u64, SyscallError> {
    check(syscall(SYS_NIC_RX_POLL, claim.0 as u64, 0, 0, 0))
}

/// Tell the kernel to refill RX buffer `buf_index` after consuming the frame.
///
/// A dropped refill costs an RX slot permanently: 256 of them and the NIC
/// stops receiving.
pub fn nic_rx_done(claim: RawHandle, buf_index: u64) -> Result<(), SyscallError> {
    check_unit(syscall(SYS_NIC_RX_DONE, claim.0 as u64, buf_index, 0, 0))
}

/// Submit the TX DMA buffer to hardware. `total_len` includes the net header.
///
/// A refused submit means the frame never goes out, which must not be
/// indistinguishable from a delivered one.
pub fn nic_tx(claim: RawHandle, total_len: u64) -> Result<(), SyscallError> {
    check_unit(syscall(SYS_NIC_TX, claim.0 as u64, total_len, 0, 0))
}

/// Allocate a TLS block for a dlopen'd module on the current thread.
///
/// The block's *virtual* address, which is what the kernel writes into the DTV.
/// `InvalidArgument` for a `module_id` of 0 or one outside the process's module
/// list, `ResourceExhausted` past `DTV_INITIAL_CAPACITY` or when the mapping
/// fails — and every user address is far below where `SyscallError` encodes, so
/// no block is ever read as one.
pub fn tls_alloc_block(module_id: u64) -> Result<u64, SyscallError> {
    check(syscall(SYS_TLS_ALLOC_BLOCK, module_id, 0, 0, 0))
}

/// A ring and where its SQ/CQ/SQE page is mapped.
///
/// **The ring owns its page and the kernel maps it at setup.** It used to hand
/// back a shared-memory token the caller mapped itself, which is the only place
/// io_uring ever needed shared memory to be a separate thing with a separate
/// lifetime.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct IoUringSetup {
    pub handle: RawHandle,
    pub _pad: u32,
    pub vaddr: u64,
}

const _: () = assert!(core::mem::size_of::<IoUringSetup>() == 16);

/// Create an io_uring instance with the given queue depth (a power of two, at
/// most 256), and map its rings.
///
/// # Safety
/// Caller must manage the returned pointer; it stops being mapped when the
/// last handle to the ring closes.
pub unsafe fn io_uring_setup(depth: u32) -> Result<(RawHandle, *mut u8), SyscallError> {
    let mut out = IoUringSetup { handle: HANDLE_INVALID, _pad: 0, vaddr: 0 };
    check_unit(syscall(
        SYS_IO_URING_SETUP,
        depth as u64,
        &raw mut out as u64,
        0,
        0,
    ))?;
    Ok((out.handle, core::ptr::with_exposed_provenance_mut(out.vaddr as usize)))
}

/// Submit SQEs and/or wait for completions on an io_uring instance.
/// `to_submit`: number of SQEs to consume from the SQ ring.
/// `min_complete`: block until at least this many CQEs are available (0 = don't block).
/// `timeout_nanos`: 0 = non-blocking, u64::MAX = block forever, else timeout in nanos.
/// Returns the number of CQEs available.
pub fn io_uring_enter(fd: RawHandle, to_submit: u32, min_complete: u32, timeout_nanos: u64) -> Result<u32, SyscallError> {
    check(syscall(SYS_IO_URING_ENTER, fd.0 as u64, to_submit as u64, min_complete as u64, timeout_nanos))
        .map(|n| n as u32)
}

// Module info (for stack unwinding / backtraces)

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
/// Returns the number of **bytes** the description needs, which is a count
/// only of bytes: the records carry packed path strings, so a module count
/// cannot size a retry. `n <= buf.len()` means the description is in the
/// buffer; `n > buf.len()` means nothing was written and `n` is what to
/// allocate. An empty buffer is therefore a size query.
///
/// The records occupy `buf[..records[0].path_offset]` — each module's path is
/// written after the last record, so the first one's `path_offset` is where
/// the array ends.
pub fn query_modules(buf: &mut [u8]) -> Result<usize, SyscallError> {
    check(syscall(SYS_QUERY_MODULES, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0)).map(|n| n as usize)
}

/// Scheduler info for the calling process.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct SchedInfo {
    /// Current vruntime of this process (nanoseconds of virtual CPU time).
    pub vruntime: u64,
    /// Global min_vruntime frontier (monotonic non-decreasing).
    pub min_vruntime: u64,
    /// Signed contract lag, frozen at the most recent runnable-state
    /// transition. Positive = process was behind the frontier when it last
    /// woke / blocked (entitled to catch up); negative = process ran ahead
    /// and will be throttled. Bounded to [-50ms, +50ms] (MAX_VRUNTIME_LAG_NS)
    /// by construction. This is the scheduler's contract, not the live
    /// `min_vruntime - vruntime` drift that accumulates while running on
    /// multi-CPU systems — compute that at the call site if you need it.
    pub lag: i64,
}

/// Get scheduler info for the calling process.
pub fn sched_info() -> SchedInfo {
    let mut info = SchedInfo { vruntime: 0, min_vruntime: 0, lag: 0 };
    syscall(SYS_SCHED_INFO, &mut info as *mut SchedInfo as u64, 0, 0, 0);
    info
}

/// Per-process accounting statistics, as [`SYS_PROCESS_STATS`] answers them.
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
    /// The process's own pid. Not authority — nothing takes a pid but
    /// [`SYS_PROCESS_OPEN`], which takes a `SysCap` beside it — but it is the
    /// name a diagnostic prints, and this is where a holder of a handle reads
    /// it. It also fills what was named padding.
    pub pid: u32,
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

/// Read accounting for the process a `Process` handle names, alive or exited.
///
/// Repeatable: the numbers are the object's, so sampling one does not spend it.
pub fn process_stats(proc: RawHandle, stats: &mut ProcessStats) -> Result<(), SyscallError> {
    check_unit(syscall(
        SYS_PROCESS_STATS,
        proc.0 as u64,
        stats as *mut ProcessStats as u64,
        core::mem::size_of::<ProcessStats>() as u64,
        0,
    ))
}
