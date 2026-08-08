# Type-safety audit: `bootloader/`, `toyos-abi/`, `toyos/`

Read-only audit, 2026-08-01, against `HEAD` = `3e2c975`. Scope is the three
crates named; `userland/`, `kernel/` and `rust/` were read only where needed to
prove that an in-scope shape is exercised, or to count a call site. Those files
are evidence and call-site arithmetic, not findings.

**Every number below came from a command.** Layouts are `rustc` output on the
declarations copied verbatim into a scratch file. Call-site counts are `grep -c`.
"The kernel can fail here" is the dispatch arm, read.

**Ranking criteria.** A finding earns its place by naming a bug the current shape
permits, or by being a clear improvement in how the code reads — judged by
writing both versions at a real call site. Effort is never an argument against.
Where a change deletes code or collapses a special case, the deleted lines and
removed runtime checks are counted and are themselves the argument. Blast radius
and fork impact are reported as *sequencing* facts — which forks break and
whether a quiet-tree window is needed — never as a reason to leave a bad type
in place. `toyos-abi` is "completely unstable" per CLAUDE.md, so there is no
compatibility debt to pay for getting a type right.

## Summary

3937 lines audited. Much of it is already at the top of the ladder:
`RingHeader`'s modulus argument, `AudioInfo`'s spelled-out padding, `Poller`'s
declared capacity, `SlotWriteGuard`/`SlotReadGuard`, `NetdConn`→`PendingResponse`,
`IoUringOp::from_raw`, and `KernelArgs`'s `offset_of!` block. Several are the
right template for the findings below.

Top four by bug class:

1. **`ipc::send<T: Copy>` publishes padding, and one live struct has 4 such
   bytes.** `StreamOpenResponse` measures 32 bytes, `shm_token@0`,
   `signal_pipe_id@8` — bytes 4..8 are padding a struct literal never writes,
   and soundd sends all 32 to every audio client. Exactly the class `4fce59c`
   closed for `AudioInfo::as_bytes`, alive in a different crate. The marker
   trait that fixes it already exists **in this same crate**, correctly
   documented, one module away, bounding a different function.
2. **`recv_payload<T: Copy>` transmutes untrusted wire bytes into an arbitrary
   type and asserts on a peer-controlled length.** A client kills its server
   with a short header.
3. **`syscall::pipe()` cannot express a failure the kernel already returns.**
   Computed: the error word decodes to `read = Fd(-1)`, `write = Fd(-8)`.
4. **`tls_alloc_block` hands an error code back as a physical address**, and its
   doc comment claims the kernel panics — which stopped being true when
   `SYS_TLS_ALLOC_BLOCK` was hardened. std does `block_phys + offset` on it.

**The unifying statement, because it predicts the next one: the untyped region
does not end at the syscall boundary — it ends wherever someone happened to
write a wrapper.** Four values cross that boundary in the same dispatch
function. `OpenFlags` is parsed into its ABI type. `SeekFrom` is parsed into a
kernel-local match with a correct default. `IoUringOp` is parsed into a
kernel-local enum whose numbering duplicates the ABI's. `DeviceType` is not
parsed at all — the kernel re-declares the five values as `u64` constants and
matches raw, with three `_ =>` arms behind them. `MmapProt` is not even read:
the kernel binds it to `_prot`.

Two findings delete more code than they add (F6, F5-delete) and one deletes
three unreachable runtime arms.

---

## F1 — `ipc::send` publishes padding; `StreamOpenResponse` leaks 4 bytes today

**Location.** `toyos/src/ipc.rs:73-77` (`send`), `:126-128` (`as_bytes`),
`toyos/src/audio.rs:27-37`. Sender: `userland/soundd/src/main.rs:432`.

**Measured** (`rustc`, declarations copied verbatim):

```
StreamOpenResponse  size=32  align=8
  shm_token@0  signal_pipe_id@8  client_period_frames@16
  client_period_bytes@20  device_sample_rate@24  device_channels@28  slot_count@30
```

Bytes 4..8 belong to no field. soundd's construction is a struct literal, so
they hold whatever its stack held; `send` does `as_bytes(payload)` over
`size_of::<T>()` and writes all 32 to the client.

**Bug permitted.** Cross-process disclosure of the sender's memory on every
stream open. And the *primitive* is unbounded: `send<T: Copy>` will do this for
any future payload, and nothing anywhere says a payload must be padding-free.

**Both ways, at `toyos/src/audio.rs:199`.**

Current — compiles for any `Copy` type, including one with padding or a pointer:

```rust
// toyos/src/ipc.rs
pub fn send<T: Copy>(fd: Fd, msg_type: u32, payload: &T) -> Result<(), IpcError> {
    let header = IpcHeader { msg_type, len: core::mem::size_of::<T>() as u32 };
    write_all(fd, as_bytes(&header))?;
    write_all(fd, as_bytes(payload))
}
fn as_bytes<T>(val: &T) -> &[u8] { /* unsafe from_raw_parts */ }

// call site
control.send(MSG_STREAM_OPEN, &req).map_err(AudioError::Ipc)?;
```

Proposed — the trait moves from `net.rs` into `ipc.rs` and bounds the primitive.
The call site is character-for-character identical:

```rust
// toyos/src/ipc.rs  (moved verbatim from net.rs:11-15, one clause added)
/// # Safety
/// Implementors must be `#[repr(C)]`, have no padding bytes, no pointers, and
/// every bit pattern must be a valid value — a peer chooses these bytes.
pub unsafe trait IpcPayload: Copy {}

pub fn send<T: IpcPayload>(fd: Fd, msg_type: u32, payload: &T) -> Result<(), IpcError> { /* body unchanged */ }
fn as_bytes<T: IpcPayload>(val: &T) -> &[u8] { /* body unchanged */ }

// call site — unchanged
control.send(MSG_STREAM_OPEN, &req).map_err(AudioError::Ipc)?;
```

and the struct grows the field the compiler already reserved:

```rust
#[repr(C)]
#[derive(Clone, Copy)]
pub struct StreamOpenResponse {
    pub shm_token: u32,
    pub _pad0: u32,
    pub signal_pipe_id: u64,
    // ... unchanged
}
unsafe impl crate::ipc::IpcPayload for StreamOpenResponse {}

const _: () = assert!(core::mem::size_of::<StreamOpenResponse>() == 4+4+8+4+4+4+2+2);
```

**Why the proposed version is better beyond the bug.** The `unsafe trait` is not
new machinery — it exists, it is correctly worded, and `toyos/src/net.rs` already
carries 15 `unsafe impl IpcPayload for …` lines (counted) for exactly this
purpose. Today those 15 impls are decoration: `NetdConn::request` requires the
bound, `Connection::send` does not, and both funnel into the same
`write_all(fd, as_bytes(payload))`. Moving the trait one module up makes the 15
existing impls load-bearing instead of ornamental. That is a reduction in the
number of things a reader has to hold: one rule for all IPC, not one rule for
netd's protocol and none for everyone else's.

**Delta.** `net.rs` loses 5 lines, `ipc.rs` gains 5 (the trait moves). Three
`unsafe impl` lines added in `audio.rs`, plus one `_pad0` field and one `const _`.
Net ≈ +6 lines. Zero call sites change. **The wire format does not change** —
`_pad0` occupies bytes the compiler already reserved; only their content becomes
defined.

**Fork impact: none.** Enumerated over `~/.cargo/git/checkouts/`: the only
`toyos::` symbols any fork names are `net::{tcp_connect, tcp_bind, tcp_accept,
tcp_close, tcp_shutdown, tcp_set_option, OPT_NODELAY, TcpSocketId, TcpAccepted}`,
`poller::{Poller::new, IORING_POLL_IN, IORING_POLL_OUT}`, and
`audio::{AudioStream::open, FORMAT_S16LE}`. No fork names `toyos::ipc`,
`IpcPayload`, `Connection::send`, or `StreamOpenResponse`. cpal is a transitive
consumer through `AudioStream::open` and gets a rebuild from `[patch]`.

---

## F2 — `recv_payload` transmutes untrusted bytes and panics on a peer-controlled length

**Location.** `toyos/src/ipc.rs:100-107`.

**Three bugs permitted, descending.**

1. **A peer kills its server with one message.** `header.len` is off the wire.
   A client sending `MSG_STREAM_OPEN` with `len = 0` fires the `assert!` inside
   soundd. The same shape reaches the compositor and netd
   (`PendingResponse::response`, `net.rs:363-366`, routes here). CLAUDE.md's
   "an `expect()` on a value that crossed the trust boundary", one boundary out
   from the kernel — a userland-triggered *daemon* panic.

   **Correction, from running it (`788decd`): the soundd half is wrong.**
   soundd never calls `recv_payload` — it frames client messages itself in
   `MsgBuf` (`soundd/src/main.rs:990-1046`), which reads to the header
   boundary, checks the declared length against
   `size_of::<StreamOpenRequest>()`, and disconnects the client on an
   oversized one. It is the only daemon that already did this. The compositor
   (four `recv_payload` sites) and netd (whose `.ok()` cannot catch a panic)
   are the real ones; measured red-before on the compositor, which panicked at
   `ipc.rs:223` on a `len = 0` `MSG_CREATE_WINDOW` and left "no compositor is
   running" behind it.
2. **`T: Copy` is not `T: FromBytes`.** Arbitrary wire bytes become a `T` with
   no validity check. Today's payloads are integer aggregates so nothing is
   unsound in practice; adding a `bool`, a fieldless enum, or a `NonZeroU32`
   makes it UB, and the signature does not say not to.
3. **`mem::zeroed::<T>()`** is UB for any `T` with no valid all-zero value.
   Same latent shape at `toyos/src/device.rs:12` (`read_info<T: Copy>`).

**Also: `header.len` is unbounded.** `skip()` (`:134-142`) loops `read_exact`
until it has consumed `header.len - count` bytes. A peer sending
`len = u32::MAX` and then nothing parks the server thread forever. Grepping
`MAX_` across `toyos/src/` and `toyos-abi/src/` returns exactly one constant —
`Poller::MAX_HANDLES` — so the SDK has no frame bound at all. Same class as
`specs/issues/`' "a client's request is an allocation request", with a thread as
the resource instead of memory.

**Both ways.**

Current:

```rust
pub fn recv_payload<T: Copy>(fd: Fd, header: &IpcHeader) -> Result<T, IpcError> {
    let size = core::mem::size_of::<T>();
    assert!(header.len as usize >= size);
    let mut val = unsafe { core::mem::zeroed::<T>() };
    read_exact(fd, as_bytes_mut(&mut val))?;
    skip(fd, header.len as usize - size)?;
    Ok(val)
}
```

Proposed:

```rust
/// A frame body cannot exceed this. Policy, sized like `user_ptr::MAX_USER_STR`:
/// large enough for every message in the tree, small enough that a hostile
/// length is a refusal rather than a parked thread.
pub const MAX_FRAME_LEN: u32 = 64 * 1024;

pub fn recv_payload<T: IpcPayload>(fd: Fd, header: &IpcHeader) -> Result<T, IpcError> {
    let size = core::mem::size_of::<T>();
    if header.len > MAX_FRAME_LEN || (header.len as usize) < size {
        return Err(IpcError::Malformed);
    }
    let mut val = core::mem::MaybeUninit::<T>::uninit();
    read_exact(fd, unsafe { as_bytes_uninit(&mut val) })?;
    skip(fd, header.len as usize - size)?;
    Ok(unsafe { val.assume_init() })   // sound: IpcPayload promises all-bit-patterns-valid
}
```

**Which reads better.** The proposed body is one line longer and says what it
means: a malformed frame is `Err(Malformed)`, not a crash. `MaybeUninit` also
removes the `mem::zeroed` that was writing a `T` twice — once with zeros, once
from the wire. The `assert!` was the only place in the SDK where a peer's number
reached a panic; deleting it removes a whole answer the type system now gives
for free.

**Call site.** `toyos/src/audio.rs:203` — `control.recv_payload(&header)` —
unchanged; inference already picks `StreamOpenResponse`. `IpcError` gains a
`Malformed` variant; all existing handlers are `map_err(|_| …)` (grep: 11 sites,
all discarding), so none needs editing.

**Fork impact: none** — same enumeration as F1; forks reach `recv_payload` only
through `toyos::net::tcp_*`, whose signatures do not change.

---

## F3 — `syscall::pipe()` cannot express a failure the kernel already returns

**Location.** `toyos-abi/src/syscall.rs:379-385`. Kernel:
`kernel/src/arch/syscall.rs:835-849`, which returns
`SyscallError::ResourceExhausted.to_u64()` on three paths.

**Computed:** `ResourceExhausted.to_u64() = 0xfffffffffffffff8`, which the
current wrapper decodes as `read = Fd(-1)`, `write = Fd(-8)`.

**Bug permitted.** Under fd-table exhaustion (`MAX_FDS = 1024`) or pipe-page
exhaustion, every caller gets two plausible negative fds. In-tree it survives by
accident on one path and not the other: `net.rs:393` immediately calls
`pipe_id(Fd(-8))`, which the kernel rejects and which `map_err`s to
`NetError::Io`; `soundd/src/main.rs:427-428` calls `syscall::pipe()` then
`.expect("pipe_id failed")`, so there the same input is a daemon panic.

**Both ways, at `toyos/src/net.rs:390-394`.**

Current — 5 lines to make two pipes, and the failure is invisible:

```rust
pub fn pipe() -> PipeFds {
    let raw = syscall(SYS_PIPE, 0, 0, 0, 0);
    PipeFds { read: Fd((raw >> 32) as i32), write: Fd((raw & 0xFFFF_FFFF) as i32) }
}

// net.rs
let rx_pipe = syscall::pipe();
let tx_pipe = syscall::pipe();
let rx_pipe_id = syscall::pipe_id(rx_pipe.write).map_err(|_| NetError::Io)?;
let tx_pipe_id = syscall::pipe_id(tx_pipe.read).map_err(|_| NetError::Io)?;
```

Proposed:

```rust
pub fn pipe() -> Result<PipeFds, SyscallError> {
    let (read, write) = halves(check(syscall(SYS_PIPE, 0, 0, 0, 0))?);
    Ok(PipeFds { read: Fd(read as i32), write: Fd(write as i32) })
}

// net.rs
let rx_pipe = syscall::pipe().map_err(|_| NetError::Io)?;
let tx_pipe = syscall::pipe().map_err(|_| NetError::Io)?;
let rx_pipe_id = syscall::pipe_id(rx_pipe.write).map_err(|_| NetError::Io)?;
let tx_pipe_id = syscall::pipe_id(tx_pipe.read).map_err(|_| NetError::Io)?;
```

(`halves` is F16's shared splitter.) The call site gains `.map_err(…)?` — the
same suffix the next two lines already carry. It reads *more* uniform, not less:
four consecutive fallible calls that now all look fallible.

**Delta.** +1 line in the ABI, +1 character-class per call site, 6 call sites in
`net.rs`, 1 in soundd, 1 in libc.

**Fork impact: YES, one site.**
`mio-cf4ccd2d6e4940e0/c3d36f6/src/sys/toyos/waker.rs:13` —
`let pipe = toyos_abi::syscall::pipe();`. Needs the quiet-tree window CLAUDE.md
describes for fork edits, and should be batched with F4 (std) so one window
covers both.

---

## F4 — `tls_alloc_block` returns an error code as an address; its doc comment is stale

**Location.** `toyos-abi/src/syscall.rs:901-906`. Kernel:
`kernel/src/arch/syscall.rs:1720-1745`. Consumer:
`rust/library/std/src/sys/pal/toyos/tls.rs:30`.

The doc says "Panics in the kernel if module_id is invalid or allocation fails."
The kernel has not panicked here since the hardening pass `specs/issues/` records at
line 245: `tls_alloc_block` returns `InvalidArgument` for `module_id == 0` and
for a module not in the process's list, and `ResourceExhausted` past
`DTV_INITIAL_CAPACITY`. `sys_tls_alloc_block` encodes them with `e.to_u64()`.

**Bug permitted.** `__tls_get_addr` computes `block_phys + offset` on a value
near `u64::MAX`, wraps, and returns a wild pointer its caller dereferences. The
symptom is a fault at an arbitrary address with nothing pointing at the DTV — and
the doc comment actively misdirects whoever debugs it, by naming a kernel panic
they would have seen.

**Both ways, at `rust/library/std/src/sys/pal/toyos/tls.rs:29-31`.**

Current:

```rust
unsafe extern "C" fn __tls_get_addr_slow(module_id: u64, offset: u64) -> *mut u8 {
    let block_phys = toyos_abi::syscall::tls_alloc_block(module_id);
    core::ptr::without_provenance_mut((block_phys + offset) as usize)
}
```

Proposed:

```rust
unsafe extern "C" fn __tls_get_addr_slow(module_id: u64, offset: u64) -> *mut u8 {
    let Ok(block) = toyos_abi::syscall::tls_alloc_block(module_id) else {
        rtabort!("__tls_get_addr: no TLS block for module {module_id}");
    };
    core::ptr::without_provenance_mut((block + offset) as usize)
}
```

**Which reads better.** An abort is correct here and the current code is
*reaching* for it — `__tls_get_addr`'s ABI is that it returns an address, there
is no caller to return an error to, and the process's own linkage is what is
wrong. The proposed version says that; the current version pretends the failure
cannot happen and then silently constructs the wrong answer. Three lines become
five and the failure acquires a name.

**Delta.** ABI: `syscall(...)` → `check(syscall(...))`, one line. std: +2 lines.
Doc comment corrected.

**Fork impact: the std fork, one call site.** Same window as F3.

---

## F5 — CLOSED, Option A: `MmapProt` is now enforced

**Resolved by implementing it.** `sys_mmap` takes `MmapProt`/`MmapFlags` rather
than two `u64`s, `contains` exists on both (so the `// MmapFlags::FIXED` comment
that stood in for the code is gone), and `prot` reaches the PDE: no `WRITE`
means a read-only mapping, and `MmapProt::NONE` maps nothing at all — the range
is reserved so nothing else lands in it, no physical memory is pinned behind a
page whose purpose is to fault, and `process::handle_page_fault` refuses to fill
a `RegionKind::Mapped` region so the reservation cannot be demand-paged back
into existence.

`mmap_prot` is the gate: three children, one per refused access (store to
`PROT_NONE`, load from `PROT_NONE`, store to `PROT_READ`), each of which must
die, against positive controls that `PROT_READ|PROT_WRITE` still reads back what
it wrote and `PROT_READ` is readable. Negative-controlled: with `writable` forced
true and the `NONE` arm disabled, the children print `SURVIVED`.

The reasoning that chose Option A over deleting the type is below, unchanged.

---

## F5 (original) — `MmapProt` is an ABI type the kernel discards

**Location.** `toyos-abi/src/syscall.rs:247-261` (`MmapProt`, 15 lines),
`:262-276` (`MmapFlags`, 15 lines). Kernel:
`kernel/src/arch/syscall.rs:1235` — `fn sys_mmap(req_addr: u64, size: u64,
_prot: u64, flags: u64)` — and `:1246` — `let fixed = flags & 4 != 0; //
MmapFlags::FIXED`. Grepping `_prot|MmapProt|MmapFlags` across `kernel/src/`
returns **exactly those two lines**.

**Census of every `MmapProt::` argument in the tree** (excluding `rust/` and the
declaration): `WRITE` ×12, `READ` ×1, `NONE` ×1 — and the single `NONE` is
`userland/libc/src/posix_io.rs:422`'s accumulator initializer, not a request.
So every real call passes `READ | WRITE`, and the kernel ignores it anyway.

**Bug permitted.** `MmapProt::NONE` — the guard-page request — produces a
readable, writable mapping. `posix_io.rs:420-424` translates POSIX `PROT_READ`/
`PROT_WRITE` into it, so a C program that mmaps a guard page gets a writable one
and its stack-overflow detection is silently gone. `MmapFlags::ANONYMOUS` and
`PRIVATE` are never read either; `FIXED` is decoded by a bare `4` whose name
appears only in a comment, so renumbering the ABI constant desynchronises the two
crates with no compiler between them.

**Two honest options. Both are better than the status quo; the choice is a
capability question, not a work question, and it is the owner's.**

**Option A — implement it.** The boundary parses once, as `SYS_OPEN` already
does:

```rust
// kernel/src/arch/syscall.rs — current
SYS_MMAP => sys_mmap(a1, a2, a3, a4),
fn sys_mmap(req_addr: u64, size: u64, _prot: u64, flags: u64) -> u64 {
    let fixed = flags & 4 != 0; // MmapFlags::FIXED

// proposed
SYS_MMAP => sys_mmap(a1, a2, MmapProt(a3), MmapFlags(a4)),
fn sys_mmap(req_addr: u64, size: u64, prot: MmapProt, flags: MmapFlags) -> u64 {
    let fixed = flags.contains(MmapFlags::FIXED);
```

The `// MmapFlags::FIXED` comment deletes because the code now says it. Needs
`contains` on both types (F15 supplies it), and `prot` plumbed to the PTE flags.

**Option B — delete it.** `MmapProt` (15 lines) and two of three `MmapFlags`
bits go; ToyOS mappings are documented as always RW:

```rust
// toyos-abi — current
pub unsafe fn mmap(addr: *mut u8, size: usize, prot: MmapProt, flags: MmapFlags) -> *mut u8

// proposed
/// Mappings are always readable and writable; ToyOS has no per-page protection.
pub unsafe fn mmap(addr: *mut u8, size: usize, fixed: bool) -> *mut u8
```

```rust
// userland/libc/src/posix_io.rs:420-428 — current, 9 lines
use toyos_abi::syscall::{MmapProt, MmapFlags};
let mut mp = MmapProt::NONE;
if prot & 1 != 0 { mp = mp | MmapProt::READ; }
if prot & 2 != 0 { mp = mp | MmapProt::WRITE; }
let mut mf = MmapFlags::PRIVATE;
if flags & 0x20 != 0 { mf = mf | MmapFlags::ANONYMOUS; }
if flags & 0x10 != 0 { mf = mf | MmapFlags::FIXED; }

// proposed, 1 line
let fixed = flags & 0x10 != 0;
```

**Counted.** Option B deletes 15 lines of `MmapProt`, 2 of 3 `MmapFlags`
constants, 8 lines of dead translation in `posix_io.rs`, and simplifies 9 call
sites from two flag arguments to one bool — a measured 45 `MmapProt|MmapFlags`
references tree-wide (excluding `rust/`) collapse to roughly 10. Option A deletes
1 comment and adds the PTE plumbing.

**My reading:** Option B is the better *code* by a wide margin and matches
CLAUDE.md's "prefer the simpler solution"; Option A is the better *system*,
because PROT_NONE guard pages are a real safety mechanism that libc programs
expect and that ToyOS silently does not provide. The one thing that must not
continue is the present state, where the type exists, is threaded through 45
references, and means nothing. **Flag: Option B narrows the syscall ABI — owner
decision.**

**Fork impact: none.** No fork names `MmapProt` or `MmapFlags`; the only
non-monorepo consumer is `rust/library/std/src/sys/alloc/toyos.rs:17-18`, which
passes `READ | WRITE` and `ANONYMOUS`.

---

## F6 — `DeviceType` is an ABI enum the kernel re-derives, behind three `_ =>` arms

**Location.** `toyos-abi/src/syscall.rs:577-586` declares
`#[repr(u64)] enum DeviceType { Keyboard = 0, …, Audio = 4 }`.
`kernel/src/device.rs:6-10` re-declares the same five numbers as `u64`
constants, and three functions match raw `u64`: `try_claim` (`:48`, `_ =>
Err(ClaimError::UnknownType)` at `:112`), `is_owner` (`:120`, `_ => return
false` at `:127`), `release` (`:132`, `_ => return` at `:140`).
`kernel/Cargo.toml:87` already depends on `toyos-abi`.

**Bug permitted.** `release(device_type, pid)` **fails silently open**: an
unrecognised type returns without releasing, so the device stays claimed forever.
`is_owner` fails closed (`return false`), which is right — and `is_owner` is the
gate CLAUDE.md names for `SYS_AUDIO_SUBMIT` and `SYS_SET_RT_PRIORITY`. Two
functions, same input domain, opposite failure directions, three lines apart.
Adding or renumbering a class means editing two files in two crates with nothing
checking the correspondence.

**Both ways, at `kernel/src/device.rs:132-142`.**

Current:

```rust
pub fn release(device_type: u64, pid: Pid) {
    let mut owner = match device_type {
        DEVICE_KEYBOARD => KEYBOARD_OWNER.lock(),
        DEVICE_MOUSE => MOUSE_OWNER.lock(),
        DEVICE_FRAMEBUFFER => FRAMEBUFFER_OWNER.lock(),
        DEVICE_NIC => NIC_OWNER.lock(),
        DEVICE_AUDIO => AUDIO_OWNER.lock(),
        _ => return,
    };
    if *owner == Some(pid) { *owner = None; }
}
```

Proposed:

```rust
pub fn release(device: DeviceType, pid: Pid) {
    let mut owner = match device {
        DeviceType::Keyboard => KEYBOARD_OWNER.lock(),
        DeviceType::Mouse => MOUSE_OWNER.lock(),
        DeviceType::Framebuffer => FRAMEBUFFER_OWNER.lock(),
        DeviceType::Nic => NIC_OWNER.lock(),
        DeviceType::Audio => AUDIO_OWNER.lock(),
    };
    if *owner == Some(pid) { *owner = None; }
}
```

with the parse at the one place the value enters the kernel:

```rust
// kernel/src/arch/syscall.rs:292 — current
SYS_OPEN_DEVICE => sys_open_device(a1),

// proposed
SYS_OPEN_DEVICE => match DeviceType::from_u64(a1) {
    Some(d) => sys_open_device(d),
    None => SyscallError::InvalidArgument.to_u64(),
},
```

**Counted delta.** Deleted: 5 `pub const DEVICE_*` lines, 3 `_ =>` arms,
`ClaimError::UnknownType` and its doc comment (2 lines), and the
`Err(ClaimError::UnknownType) => …` arm at `arch/syscall.rs:1051`. That is
**11 lines and 3 runtime checks out**. Added: `DeviceType::from_u64` (~10 lines
in `toyos-abi`, template already present at `FileType::from_u64`,
`syscall.rs:199-215`) and 3 dispatch lines. Roughly break-even on lines, and
**three catch-all arms become compile-time exhaustiveness** — a sixth device
class becomes an error at all three sites instead of a silently-not-released
device.

**Fork impact: none.**

---

## F7 — bare ids across the ABI: what the capability spec covers, and what it misses

Full enumeration of integer-as-authority in the audited crates:

| Item | Location | Spec disposition |
|---|---|---|
| `FramebufferInfo.token: [u32;2]`, `.cursor_token` | `toyos-abi/src/lib.rs:65-66` | **Covered** — §6.5, `SharedMem` handles at `SYS_OPEN_DEVICE` |
| `NicInfo.dma_token` | `toyos-abi/src/net.rs:6` | **Covered** — §6.5 |
| `AudioInfo.dma_token` | `toyos-abi/src/audio.rs:17` | **Covered** — §6.5 |
| `io_uring_setup` → `(Fd, u32)` | `syscall.rs:911-916` | **Covered** — §6.8 |
| `alloc/grant/map/release_shared` token | `syscall.rs:626-672` | **Covered** — §6.2, deleted at stage D |
| `StreamOpenResponse.shm_token`, `.signal_pipe_id` | `toyos/src/audio.rs:30-31` | **Covered** — §8.3 is written around this message |
| `pipe_open`/`pipe_id`/`socket_create` | `syscall.rs:844-858` | **Covered** — §7, deleted |
| `dl_open` → `u64`, `dl_close(u64)` | `syscall.rs:738-767` | **NOT covered** — `KObjectRef` has no module variant |
| `TcpSocketId(pub u32)`, `UdpSocketId(pub u32)` | `toyos/src/net.rs:121-125` | **NOT covered** |

**The `TcpSocketId` gap is a live authority hole the spec would not close.**
`netd`'s `alloc_id` (`userland/netd/src/main.rs:372-376`) is
`let id = self.next_id; self.next_id += 1; id` — dense and sequential — into a
single global `self.sockets` map. Every operation in `toyos/src/net.rs` opens a
**fresh** `NetdConn::connect()` (`:499, 505, 511, 517, 585`), so netd has no
connection to scope ownership to even if it wanted one; `tcp_close`,
`tcp_shutdown`, `udp_close`, `tcp_get_option`, `tcp_set_option` all look up
`req.socket_id` with no check of who is asking (`main.rs:418, 452, 522, 618,
679, 703`). `for id in 0.. { tcp_close(TcpSocketId(id)) }` closes every TCP
socket on the machine.

This is `specs/issues/isolation/`'s class exactly, in a userland daemon's namespace, and
§7 of the spec tabulates kernel integers only. The spec supplies the mechanism
(`SYS_HANDLE_SEND`/`RECV`) and assigns no obligation; no stage A–F touches netd.

**Both ways, and the SDK shape is what has to change first.**

```rust
// toyos/src/net.rs — current: an integer is the authority, and every call
// re-connects, so netd cannot scope it even in principle.
pub struct TcpConnection { pub rx: Pipe, pub tx: Pipe, pub socket_id: TcpSocketId, pub local_port: u16 }
pub fn tcp_close(socket_id: TcpSocketId) -> Result<(), NetError> {
    NetdConn::connect()?
        .request(MsgType::TcpClose, &SocketCloseRequest { socket_id: socket_id.0 })?
        .status()
}

// proposed: the connection to netd *is* the socket's authority and lives as
// long as the socket. Close is Drop.
pub struct TcpConnection { pub rx: Pipe, pub tx: Pipe, ctl: NetdConn, pub local_port: u16 }
impl Drop for TcpConnection { fn drop(&mut self) { let _ = self.ctl.signal(MsgType::TcpClose); } }
```

That deletes `TcpSocketId`, `UdpSocketId`, `SocketCloseRequest`, the
`socket_id` field from 9 of the 15 protocol structs in `net.rs`, and the free
functions `tcp_close`/`udp_close` — and it deletes netd's `sockets` map key
lookup on every one of the six handlers listed above, because the handler now
knows which connection it arrived on. It is a larger change than anything else
in this audit and it is the one that most reduces code.

**Fork impact: YES.** `socket2` and `mio` name
`toyos::net::{tcp_connect, tcp_bind, tcp_accept, tcp_close, tcp_shutdown,
tcp_set_option, TcpSocketId, TcpAccepted}`. Two forks, quiet-tree window.

---

## F8 — `SharedMemory` conflates owning a region with mapping someone else's

**Location.** `toyos/src/shm.rs` (61 lines total).

**Four bugs, one type.**

1. **`allocate` and `map` produce the same type and the same `Drop`.** Two
   `SharedMemory::map(t)` for one token means two releases;
   `sys_release_shared` (`kernel/src/arch/syscall.rs:1226-1233`) returns
   `NotFound` on the second, and `release_shared` turns that into
   `assert_eq!(result, 0, …)` — a panic in a destructor.
2. **`size` is the caller's word**, never validated against the mapping.
   `AudioStream::open` (`toyos/src/audio.rs:208-210`) computes it from
   `resp.slot_count` and `resp.client_period_bytes`, both off the wire, and
   `slot_data_mut` (`:88-93`) builds slices inside it.
3. **`as_mut_slice(&mut self) -> &mut [u8]` over cross-process shared memory.**
   `&mut` asserts exclusivity nothing provides — the peer is writing the same
   physical page. Live users: `compositor/src/main.rs:951, 1518`,
   `window/src/lib.rs:217`. `RingHeader` and `AudioSlotHeader` get this right by
   exposing only atomics; `SharedMemory` hands out plain slices.
4. **Three infallible signatures over fallible syscalls**: `allocate` →
   `alloc_shared` asserts (`syscall.rs:629`), `grant` → `.expect` (`shm.rs:37`),
   `Drop` → `release_shared` asserts.

**Both ways, at `toyos/src/audio.rs:210`.**

```rust
// current — the type cannot tell you whether dropping it destroys a region
// you own or merely unmaps someone else's, and `shm_size` is unchecked.
let shm = SharedMemory::map(resp.shm_token, shm_size);

// proposed
let shm = MappedShm::map(resp.shm_token, shm_size)
    .map_err(|e| AudioError::Ipc(IpcError::Syscall(e)))?;
```

```rust
pub struct OwnedShm { token: u32, ptr: *mut u8, len: usize }   // Drop: destroy region
pub struct MappedShm { token: u32, ptr: *mut u8, len: usize }  // Drop: unmap only

impl OwnedShm {
    pub fn allocate(len: usize) -> Result<Self, SyscallError>;
    pub fn grant(&self, pid: Pid) -> Result<(), SyscallError>;
}
impl MappedShm {
    /// `len` is checked against the region the kernel actually mapped.
    pub fn map(token: u32, len: usize) -> Result<Self, SyscallError>;
}
// neither exposes `as_mut_slice`
pub fn as_cells(&self) -> &[core::cell::Cell<u8>];
```

**Which reads better.** Two types with one job each, and the double-release stops
being expressible. The three `assert!`/`expect` sites become `?`, which is the
same length. The `as_cells` accessor is the honest signature for memory another
process writes, and it is what the `Ring`/`AudioSlot` types in this same crate
already model with atomics.

**Spec sufficiency.** §6.2 makes the *ownership* half unrepresentable —
mappings hold `Arc<SharedMemObject>`, there is no `destroy()`, the double release
cannot be expressed. It does **not** address the length half:
`SYS_SHM_MAP(h) -> vaddr` (§11) still returns a bare address with the length
arriving separately, so a wire-derived `shm_size` remains buildable past the
mapping. Nor the `&mut [u8]` aliasing. `SYS_SHM_MAP(h) -> (vaddr, len)` and an
`as_cells` in §10 would close both.

**Fork impact: none** — no fork names `toyos::shm`.

---

## F9 — unchecked syscall wrappers, and a checked twin with zero callers

Every wrapper in `toyos-abi/src/syscall.rs` that does not call `check`, against
the dispatch arm:

| Wrapper | Line | Kernel can fail? | Consequence |
|---|---|---|---|
| `pipe()` | 379 | **yes**, 3 paths | F3 |
| `tls_alloc_block()` | 904 | **yes**, 2 codes | F4 |
| `get_env()` | 389 | **yes** — `return bad_addr`, `arch/syscall.rs:399` | error word returned as a byte count near `1.8e19`; caller slices `buf[..n]` |
| `waitpid()` | 403 | **yes** — `NotFound`, `WouldBlock` (`:1029, 1035`) | error word returned as an exit code |
| `sysinfo()` | 676 | yes | folded to `0`; reason lost, and `0` is also "nothing written" |
| `getcwd()` | 508 | yes | folded to `0`, **documented** as the error return — the honest one here |
| `mmap()` | 812 | yes | folded to null — the pointer idiom, fine |
| `random()` | 514 | **no** — `sys_random` returns `0` unconditionally (`:710-722`) | honest today; signature still cannot report a future failure |
| `close`, `mark_tty`, `nanosleep`, `set_thread_name`, `dl_close`, `thread_spawn`, `thread_join` | — | varies | ignored by construction |

**`waitpid` is the sharp one, and it deletes.** `waitpid_flags` — the checked
twin — has **zero callers** anywhere in the tree (counted: `grep -rn waitpid`
excluding `toyos-abi` and `rust/`'s unrelated libc uses). The unchecked
`waitpid` has exactly two: `userland/libc/src/misc.rs:115` and
`rust/library/std/src/sys/process/toyos.rs:401`. So the ABI carries two
functions, and the live one is the one that throws the answer away.

**Both ways, at `rust/library/std/src/sys/process/toyos.rs:399-402`.**

```rust
// current — waitpid on a non-child returns NotFound.to_u64(); `as i32` makes
// that -2, and `Ok(ExitStatus(-2))` is indistinguishable from a real exit.
pub fn wait(&mut self) -> io::Result<ExitStatus> {
    let code = toyos_abi::syscall::waitpid(toyos_abi::Pid(self.pid));
    Ok(ExitStatus(code as i32))
}

// proposed — one function in the ABI, and the failure has a name
pub fn wait(&mut self) -> io::Result<ExitStatus> {
    let code = toyos_abi::syscall::waitpid(toyos_abi::Pid(self.pid), 0)
        .map_err(|_| io::const_error!(io::ErrorKind::NotFound, "no such child"))?;
    Ok(ExitStatus(code as i32))
}
```

**Counted delta.** Delete `waitpid` (`syscall.rs:402-405`, 4 lines) and rename
`waitpid_flags` → `waitpid`: **4 lines and one duplicate concept out**, 2 call
sites gain a `?`. `get_env` gains `check`, 1 line.

Separately: **nothing in this file is `#[must_use]`.** `Result` carries it for
most, but `set_rt_priority` (`:691`) warns *in prose* that discarding the result
silently drops the caller out of the RT band. Make it mechanical:

```rust
#[must_use = "a discarded refusal drops this thread out of the RT band silently"]
pub fn set_rt_priority(enable: bool) -> Result<(), SyscallError>
```

**Fork impact.** `get_env` and `waitpid`: **std only**, same window as F3/F4.
Note that `random`, `stack_info`, `futex_wait`, `futex_wake`, `write_nonblock`,
`close`, `map_shared`, `io_uring_setup`, `io_uring_enter`, `dl_open`, `dl_sym`,
`dl_close` and `Fd` *are* fork-visible — relevant to any later pass.

---

## F10 — bootloader: `alloc_page` has no bound against `PT_PAGES`

**Location.** `bootloader/src/main.rs:359-364`; budget at `:399`.

```rust
const PT_PAGES: usize = 12;
let mut next_page = 0usize;
let mut alloc_page = |pt_mem: *mut u8| -> *mut u64 {
    let p = pt_mem.add(next_page * 4096) as *mut u64;
    next_page += 1;
    p
};
```

`next_page` is never compared to `PT_PAGES`. The one call passes 4 GiB, using
3 + 4 = 7 pages of 12. The comment at `:397-398` already reasons about 8 GB and
11 pages — the author was thinking about raising it.

**Bug permitted.** Raising the mapped size past 9 GB writes page-table entries
past the allocation, into the UEFI pool, before `ExitBootServices` with firmware
live. The store is `*pd.add(pdi) = phys | flags`, so it is silent heap corruption
whose symptom is a triple fault after `mov cr3`. This is `b362082`'s class
reached from a constant instead of from a file.

**Both ways.**

```rust
// current — `alloc_page` returns a raw pointer and the budget is a comment
let mut alloc_page = |pt_mem: *mut u8| -> *mut u64 {
    let p = pt_mem.add(next_page * 4096) as *mut u64;
    next_page += 1;
    p
};
// ...
*pd.add(pdi as usize) = phys | PAGE_PRESENT | PAGE_WRITE | PAGE_SIZE_BIT;

// proposed — the budget is the type, and the entry index is bounded too
type PageTable = [u64; 512];
unsafe fn build_boot_page_tables(pt: &mut [PageTable; PT_PAGES], size: u64) -> u64 {
    let mut next_page = 0usize;
    let mut alloc_page = |pt: &mut [PageTable; PT_PAGES]| -> *mut PageTable {
        let p = &raw mut pt[next_page];   // panics by name if the budget is short
        next_page += 1;
        p
    };
    // ...
    (*pd)[pdi as usize] = phys | PAGE_PRESENT | PAGE_WRITE | PAGE_SIZE_BIT;
```

**Which reads better.** `*pd.add(i) = …` and `(*pd)[i] = …` are the same length,
and the second cannot leave the page. The `PT_PAGES: usize = 12` constant stops
being a number the reader has to check by hand against a comment about 8 GB —
the compiler checks it, and an overrun is a named panic in a file whose stated
policy (`:23-34`) is that every check ends in a named panic.

**Delta.** ~4 lines changed in one function, one caller. **Fork impact: none.**

---

## F11 — bootloader: `Vec<u8>` over uninitialized memory, and a `Vec` whose layout does not match its allocation

**Location.** `bootloader/src/main.rs:36-42` (`alloc_kernel_memory`),
`:101-106` (`alloc_uninit`).

1. **`alloc_kernel_memory` allocates with `align = 2 MiB` and wraps it in
   `Vec::from_raw_parts`.** `Vec<u8>` deallocates with `Layout::array::<u8>`,
   align 1. Dropping that `Vec` is UB. It is sound today only because every such
   `Vec` reaches `mem::forget` at `:464-467` — a whole-program property with
   nothing enforcing it, on a path that already has four `mem::forget`s in a row.
2. **`alloc_uninit` returns a `Vec<u8>` over memory that was never
   initialized.** `file.read(&mut bytes)` takes `&mut [u8]` of uninit bytes,
   which is UB by the letter. The comment at `:92-100` justifies not zeroing on
   perf grounds and is right to — `MaybeUninit` keeps the perf and the soundness.

**Both ways.**

```rust
// current — two allocators, both handing back a Vec that must never be dropped
fn alloc_kernel_memory(size: usize) -> vec::Vec<u8> { /* align 2 MiB, from_raw_parts */ }
fn alloc_uninit(size: usize) -> vec::Vec<u8> { /* align 1, uninit, from_raw_parts */ }
let read = file.read(&mut bytes).expect("Failed to read file");
assert_eq!(read, size, "short read: {read} of {size} bytes");

// proposed — one type, freed with the layout it was allocated with, and the
// short-read assert is what unlocks the initialized view
struct Image { ptr: NonNull<u8>, layout: Layout, len: usize }
impl Image {
    fn uninit(len: usize, align: usize) -> Self;
    fn spare(&mut self) -> &mut [MaybeUninit<u8>];
    /// # Safety: every byte must have been written.
    unsafe fn filled(&self) -> &[u8];
}
impl Drop for Image { /* dealloc with self.layout */ }

let read = file.read_uninit(image.spare()).expect("Failed to read file");
assert_eq!(read, size, "short read: {read} of {size} bytes");
let bytes = unsafe { image.filled() };
```

**Which reads better.** One type instead of two near-identical allocators, and
`mem::forget(image)` at `:464-467` stays exactly as it is — but now for a type
whose `Drop` would have been *correct* if it ever ran, rather than one whose
`Drop` is UB and is avoided by convention. The `assert_eq!` at `:87` stops being
a comment about why the tail is not garbage and becomes the precondition of
`filled()`.

**Delta.** Two functions (12 lines) become one type (~20 lines); three call
sites. **Fork impact: none.**

Distinct from `specs/issues/`' "the bootloader sizes every allocation from a file
the ESP handed it" — that entry is about the *bound*, which `MAX_ESP_FILE` now
supplies; this is about the *type* the bounded allocation comes back in.

---

## F12 — `gop_pixel_format` is a bare `u32` whose encoding lives in three prose comments

**Location.** `bootloader/src/main.rs:323-327` writes it
(`Rgb => 0, Bgr => 1, _ => return None`); `toyos-abi/src/boot.rs:22` carries it;
`kernel/src/drivers/panic_console/mod.rs:732-740` decodes it with a comment
citing `bootloader/src/main.rs` as the authority;
`kernel/src/drivers/virtio_gpu.rs:495` hardcodes `pixel_format: 1, // BGR`; it
travels on into `toyos_abi::FramebufferInfo.pixel_format` and out to the
compositor.

**Bug permitted.** The decode is `if fb.format == 0 { RGB } else { BGR }` —
every value that is not 0 is BGR. A third format added at either end paints wrong
colours with no error anywhere, on the panic console, which is the only output
channel a mute machine has. Four crates share a mapping nothing checks.

**Both ways, at `kernel/src/drivers/panic_console/mod.rs:732-740`.**

```rust
// current — the encoding is a comment, and the decode has an implicit else
/// `pixel_format` is 0 for RGB and 1 for BGR (`bootloader/src/main.rs`). As a
/// little-endian u32 that is the byte order of the low three bytes.
fn rgb(fb: &Fb, r: u32, g: u32, b: u32) -> u32 {
    if fb.format == 0 { r | (g << 8) | (b << 16) } else { b | (g << 8) | (r << 16) }
}

// proposed — the encoding is the type, and there is no else
fn rgb(fb: &Fb, r: u32, g: u32, b: u32) -> u32 {
    match fb.format {
        PixelFormat::Rgb => r | (g << 8) | (b << 16),
        PixelFormat::Bgr => b | (g << 8) | (r << 16),
    }
}
```

```rust
// toyos-abi/src/boot.rs — both crates already depend on it
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat { Rgb = 0, Bgr = 1 }
impl PixelFormat {
    pub fn from_u32(v: u32) -> Option<Self> {
        match v { 0 => Some(Self::Rgb), 1 => Some(Self::Bgr), _ => None }
    }
}
```

`KernelArgs.gop_pixel_format` stays `u32` — it is a `#[repr(C)]` handoff field
and an enum there would make an out-of-range value UB to *read*. The parse
happens once, where the kernel builds its `Fb` (`mod.rs:289`).

**Which reads better.** The three-line doc comment deletes, because the code
says it. The bootloader's `PixelFormat::Rgb => 0` becomes
`PixelFormat::Rgb`, and `virtio_gpu.rs:495`'s `pixel_format: 1, // BGR` becomes
`PixelFormat::Bgr` with the comment deleted. **Counted: 3 explanatory comment
lines and 1 trailing `// BGR` out**, one `else` branch becomes an exhaustive
match.

**Same treatment, same file: `boot_partition_present: u32`.** The invariant
"present == 0 implies the other three are zero" is stated in a doc comment
(`boot.rs:41-47`) and enforced only by `start_kernel`'s `match &boot_part`
(`:420-424`). The parse-once seam is an accessor on the ABI type:

```rust
impl KernelArgs {
    pub fn boot_partition(&self) -> Option<BootPartition> {
        (self.boot_partition_present != 0).then(|| BootPartition {
            guid: self.boot_partition_guid,
            start_lba: self.boot_partition_start_lba,
            blocks: self.boot_partition_blocks,
        })
    }
}
```

Every kernel consumer then reads an `Option` instead of re-deriving the
invariant from four fields.

**Another agent is editing `bootloader/src/main.rs` and `toyos-abi/src/boot.rs`
right now for GPT boot-device identity.** This is a shape observation about the
`present` flag, not a claim about the lines as they stand; hand it to whoever
lands that work rather than patching around them.

**Fork impact: none** — no fork names `toyos_abi::boot`.

---

## F13 — the `KernelArgs` layout assertion is the template, and half the structs that need it lack one

**What is right.** `toyos-abi/src/boot.rs:60-71` asserts seven field offsets, the
size (184) and the alignment (8). **Verified that those assertions cover the real
consumer**: `kernel/src/main.rs:221-223` reads `[rdi + 16]`, `[rdi + 32]`,
`[rdi + 40]` from naked asm before Rust runs, and the block asserts exactly
`kernel_memory_addr == 16`, `kernel_stack_addr == 32`, `kernel_stack_size == 40`.
This is the strongest single thing in the audited code.

**What is missing.** Measured layouts of every `#[repr(C)]` type crossing a
boundary, and whether an assertion protects it:

| Type | size/align | padding | assertion |
|---|---|---|---|
| `AudioInfo` | 52/4 | none (spelled out) | **yes** — `audio.rs:30-33` |
| `AudioCompletionRecord` | 16/8 | none (`_pad`) | **yes** — `audio.rs:65` |
| `KernelArgs` | 184/8 | 4 trailing | **yes** — `boot.rs:60-71` |
| `MemoryMapEntry` | 24/8 | **4 at offset 4** | no |
| `FramebufferInfo` | 32/4 | none | no — a SAFETY comment at `lib.rs:82` *claims* "no padding" |
| `NicInfo` | 24/4 | none | no |
| `RawKeyEvent` | 8/1 | none | no |
| `MouseEvent` | 6/2 | none | no |
| `Stat` | 24/8 | none | no |
| `ProcessStats` | 128/8 | none (`_pad`) | no |
| `ModuleInfo` | 40/8 | none | no |
| `SpawnArgs` | 48/8 | none | no |
| `IoUringSqe`/`Cqe`/`Params` | 40/16/40 | none (`_pad`) | no |
| `StreamOpenResponse` | 32/8 | **4 at offset 4** | no — **F1** |

`MemoryMapEntry`'s padding is not a leak: bootloader→kernel is trusted both ways
and the kernel reads named fields (`main.rs:253-257`). It is listed because the
asymmetry is the finding — `KernelArgs` and `MemoryMapEntry` are declared seven
lines apart in the same file, cross the same boundary, and one has the discipline.

**Both ways, at `toyos-abi/src/lib.rs:82`.**

```rust
// current — a SAFETY comment asserting a property nothing checks
// SAFETY: FramebufferInfo is #[repr(C)] and contains only u32 fields — no padding, no pointers.
unsafe impl Sync for FramebufferInfo {}

// proposed — the comment becomes the check
const _: () = assert!(core::mem::size_of::<FramebufferInfo>() == 8 + 4*6);
unsafe impl Sync for FramebufferInfo {}
```

**Which reads better.** A `SAFETY` comment that states a checkable fact and does
not check it is the weakest form in the codebase; `AudioInfo` already shows the
strong form four files away. Adding a field of the wrong type becomes E0080
instead of a silent leak.

**Delta.** ~8 four-line `const _` blocks in `toyos-abi`, zero runtime cost, zero
API change, zero fork impact. **This is what makes F1 non-recurring**, so it
should land first regardless of anything else.

---

## F14 — `Fd` is `i32` where the kernel is `u32`, and the SDK's RAII is one `.fd()` deep

**Location.** `toyos-abi/src/lib.rs:15` — `pub struct Fd(pub i32)`. Kernel:
`kernel/src/fd.rs:179-189` — `get/get_mut/remove(&self, fd: u32)`, reached via
`a1 as u32` in every dispatch arm.

**Part one: the two sides disagree about the domain.** Half of `Fd`'s range is
unrepresentable on the kernel side, and the conversion is a lossy `as`. `Fd(-1)`
becomes `4294967295` — which is the only reason F3's failure degrades into
`NotFound` rather than into a valid descriptor. `pub i32` also means `Fd(-1)`
compiles anywhere.

**Part two: the typed layer is decorative.** `toyos::Handle`
(`toyos/src/lib.rs:34-60`) is exactly right — `pub(crate)`, non-`Copy`, `Drop`
closes. But `Listener::fd` (`:66`), `Device::fd` (`:77`), `Pipe::fd` (`:92`) and
`Connection::fd` (`ipc.rs:31`) all return the `Copy` `Fd`, and 7 of `ipc.rs`'s
12 functions take a raw `Fd`. So `syscall::close(conn.fd())` compiles while
`conn` is alive and will close again on drop.

**Counted: 50 `.fd()` call sites** across `userland/` and `toyos/`
(netd 7, compositor 14, terminal 1, window 1, toyos itself 27). Real consumers
leave the typed layer immediately and stay out: `netd/src/main.rs:534, 595, 915,
928, 971, 1017` drive raw syscalls off `.fd()`; `compositor/src/main.rs:811-813`
uses raw fd numbers as poller tokens and `:1499, 1505, 1522` uses
`w.fd.fd() == client_fd` as *window identity* — safe only because `IdMap::insert`
is a monotonic counter and fd numbers are never reused, a property recorded in
the spec's §2 and in no comment near the compositor.

Inside the SDK itself, `AudioStream` (`toyos/src/audio.rs:182-189`) holds
`signal_fd: toyos_abi::Fd` raw with a hand-rolled `Drop` at `:277-281`, rather
than the `Pipe` the crate already has.

**Both ways, at `userland/netd/src/main.rs:534`.**

```rust
// current — the typed Pipe is unwrapped to a Copy Fd and a free syscall
let n = match toyos_abi::syscall::read(pipes.tx_read_fd.fd(), &mut buf) { … };

// proposed — Pipe::read already exists (toyos/src/lib.rs:94) and is unused here
let n = match pipes.tx_read_fd.read(&mut buf) { … };
```

and the escape closes with a borrowed form:

```rust
#[derive(Clone, Copy)]
pub struct BorrowedFd<'a> { raw: Fd, _p: PhantomData<&'a Handle> }
pub trait AsHandle { fn as_handle(&self) -> BorrowedFd<'_>; }

// ipc.rs free functions take BorrowedFd<'_>; syscall::close still takes Fd,
// so `syscall::close(conn.fd())` no longer compiles.
```

**Which reads better, and what it deletes.** `pipes.tx_read_fd.read(&mut buf)` is
shorter than `syscall::read(pipes.tx_read_fd.fd(), &mut buf)` and does not
mention a module the caller otherwise has no business importing —
`toyos_abi::syscall` appears **7 times in netd** and **14 in the compositor**
solely to reach around the SDK. Every one of those is a line that gets shorter.
The `.fd()` accessors themselves (4 methods) delete once `AsHandle` returns a
borrow. The `Fd(pub i32)` → `Fd(u32)` retype removes the `as u32` in every kernel
dispatch arm.

**Spec sufficiency.** §10 defines `OwnedHandle` as `!Copy` with `Drop` and calls
`into_raw` "the ONLY leak door" — but defines no borrowed form, so all 50
`.fd()` sites carry straight over onto `OwnedHandle` and the guarantee is
cosmetic one rename later. **This is the correction I would make to the spec text
before stage B3.** Related: §4.1's `pub struct RawHandle(pub u32)` repeats
`Fd(pub i32)`'s public field and should be private with `raw()`/`from_raw`.

**Fork impact: YES.** `toyos_abi::Fd` is named by 5 fork files, and mio holds
`self.fd` in its selector (`mio/src/sys/toyos/selector.rs:106`). Fork window.

---

## F15 — `OpenFlags::contains` is `intersects`

**Location.** `toyos-abi/src/syscall.rs:235`.

```rust
pub const fn contains(self, flag: Self) -> bool { self.0 & flag.0 != 0 }   // current
pub const fn contains(self, flag: Self) -> bool { self.0 & flag.0 == flag.0 } // proposed
```

**Bug permitted.** `flags.contains(READ | WRITE)` returns `true` when only
`READ` is set. **Verified: all four call sites (`kernel/src/fd.rs:202-205`) pass
single bits, so nothing is wrong today** — this is a trap, not a defect. It is
here because the name promises the opposite of what it does, and because
`MmapProt`/`MmapFlags` have no `contains` at all, which is why F5's kernel side
reads `flags & 4`.

**Delta.** Two characters, plus the same method on `MmapProt`/`MmapFlags` if F5
takes Option A. Behaviour at all four current call sites is identical.
**Fork impact: none.**

---

## F16 — packed multi-value returns share the error-encoding band

**Locations, counted:** 8 hand-rolled shift/mask lines across 4 wrappers —
`pipe` (`:382-383`), `clock_realtime` (`:527-529`, three halves),
`accept` (`:615-616`), `io_uring_setup` (`:913-914`).

`SyscallError::from_u64` claims everything `>= u64::MAX - 255`. Each of these
packs two values into the success word and relies on an unstated range argument
to stay clear of that band. `nic_rx_poll` (`:868-870`) states its argument
explicitly — "tops out at `(255 << 16) | 4096`" — and is the standard the other
four should meet. `accept` packs `client_pid << 32 | fd`, and `Pid::MAX` is
`u32::MAX` (`lib.rs:22`), so the collision is reachable in principle. All four
are unreachable in practice, and three of four say nothing about why.

**Both ways, at `accept`.**

```rust
// current — 9 lines, hand-rolled split, silent range assumption
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

// proposed A — shared splitter, 4 lines; the assumption still exists but is
// stated once instead of four times
fn halves(raw: u64) -> (u32, u32) { ((raw >> 32) as u32, raw as u32) }

pub fn accept(listener_fd: Fd) -> Result<AcceptResult, SyscallError> {
    let (client_pid, fd) = halves(check(syscall(SYS_ACCEPT, listener_fd.0 as u64, 0, 0, 0))?);
    Ok(AcceptResult { fd: Fd(fd as i32), client_pid })
}

// proposed B — out-parameter, the shape fstat/stack_info/sched_info already use;
// the success word is a plain status and the collision is unrepresentable
pub fn accept(listener_fd: Fd) -> Result<AcceptResult, SyscallError> {
    let mut out = AcceptResult { fd: Fd(0), client_pid: 0 };
    check_unit(syscall(SYS_ACCEPT, listener_fd.0 as u64, &mut out as *mut _ as u64, 0, 0))?;
    Ok(out)
}
```

**Which reads better.** B, clearly: 4 lines, no shifts, no masks, no range
argument to maintain, and `AcceptResult` becomes a `#[repr(C)]` struct the kernel
writes through a checked user pointer — the same mechanism `fstat` uses three
functions earlier in the file. It is also the only one of the three that makes
the aliasing *unrepresentable* rather than merely documented. A deletes 8 shift
lines and closes nothing; B deletes 8 shift lines and closes the class.

**This changes the return convention of 4 syscalls. Flag: owner decision** per
CLAUDE.md's "never add or change a syscall without discussion."

**Fork impact: YES** — `io_uring_setup` and `pipe` are both named by mio.

---

## Examined and deliberately not flagged

- **`toyos-abi/src/ring.rs`.** The `modulus()` = `2 * capacity` argument is
  spelled out at `:44-57` and gated by three host tests, one of which actually
  walks the cursor past `2^32`. `capacity: u32` is non-atomic and written once in
  `init`. Nothing to improve. (Known-issues §263 tracks a separate wrap concern;
  not re-filed.)
- **`SyscallError::from_u64`'s `_ => Some(Self::Unknown)`.** Correct
  forward-compatibility — an older binary meeting a newer kernel gets "an error,
  reason unrecognised" rather than a decode failure. Same judgement as
  `NetError::from_error_code`'s `code => Protocol(code)` arm, which documents it.
- **`AudioSlotHeader`'s private `_pad0`/`_pad1`.** Privacy is load-bearing: the
  struct is only reached through a pointer cast into shared memory, and a private
  field makes a struct literal impossible. Right call.
- **`SlotWriteGuard` / `SlotReadGuard`** (`toyos/src/audio.rs:59, 152`). Already
  the typestate the brief asks for: the guard mutably borrows the writer so a
  second slot cannot be handed out, `commit`/`advance` consume it, and the index
  is captured at peek time so a hostile peer rewinding `write_idx` cannot abort
  soundd. The model the rest of the SDK should be measured against.
- **`NetdConn` → `PendingResponse`** (`toyos/src/net.rs:315-381`). `request`
  consumes the connection; you cannot read a response you did not ask for. (F7 is
  about the *id*, not this.)
- **`Poller`** (`toyos/src/poller.rs`). Capacity is a declaration with an
  assertion, the sizing rule is derived and stated, `dropped` is read on every
  wait. The only remark worth making is that `MAX_HANDLES = 256` is the sole
  `MAX_` constant in the entire SDK — a fact about the rest of the SDK, not about
  `Poller`.
- **`IoUringOp::from_raw`.** The ABI's claim at `io_uring.rs:1-2` that "the
  kernel converts to a type-safe enum at the syscall boundary" is **verified
  true** — `kernel/src/io_uring.rs:68-79`, `Result<Self, SyscallError>` with an
  `InvalidArgument` default. The residual is that `0..=4` is written twice, which
  is F6's shape at a tenth of the stakes; folding it in would be right if F6 lands.
- **`FileType::from_u64`, `MsgType::from_u32`, `NetError::from_error_code`, the
  `SeekFrom` decode** (`arch/syscall.rs:218-224`, with an `InvalidArgument`
  default). All correct parse-at-the-edge.
- **The bootloader's panic-on-everything policy** (`main.rs:23-34`). Deliberate
  and justified: nothing has a caller to return to. F10 and F11 ask for *better*
  panics, not for error returns.
- **`KernelArgs` passed by reference into the kernel.** The kernel copies it to
  its own stack immediately (`main.rs:249-250`) with the reason stated. Not a
  type problem.
- **`Pid`/`Tid` `impl Add`** (`lib.rs:33-36, 55-58`). `Pid + Pid` is meaningless
  but names no bug and is presumably serving an iteration site.
- **`Fd` as a map key and poller token in the compositor.** Real, and safe only
  because fd numbers are never reused. `userland/` is out of scope, and the
  spec's §4.1 generation tagging is the answer.

---

## Is `specs/capability-handles-spec.md` sufficient — and would a smaller design read better?

### Where it is sufficient

**For the id-as-capability half of the ABI: comprehensively.** Every bare kernel
token in `toyos-abi/` and `toyos/` — both framebuffer tokens, the cursor token,
both DMA tokens, the io_uring token, the shm token and pipe id in
`StreamOpenResponse`, the whole `pipe_open`/`pipe_id`/`socket_create` family —
has a named disposition in §6, §7 or §8.3, and §8.3 is written around the exact
message this audit measured a padding leak in. §11 is a net **−1 syscall**
(11 new, 12 deleted), which is the right direction.

**Two pieces I looked at for a smaller alternative and would keep as written:**

- **§5.2's deferred zero-handle queue.** It is the largest piece of new
  machinery in the spec (per-CPU queue, three drain sites, `retired: AtomicBool`,
  a resurrection assert). The obvious reduction — "just require `on_zero_handles`
  to take no locks" — is a discipline rule, which is precisely what the spec
  rejects and precisely the class that has already bitten this codebase
  (`specs/issues/panic-path/`). The machinery earns its place.
- **§6.1's two pipe-end types.** `PipeReadEnd` with no write method makes
  "write to a read end" a compile error instead of a `Rights` check. This is
  strictly better than the `Rights::READ`/`WRITE` bits it partly duplicates, and
  it is what F14's SDK half should copy.

### Where it is not sufficient

1. **`#[repr(C)]` padding (F1, F13) is invisible to the whole design.** Handles
   change who may *name* an object; they do not change what bytes a struct
   publishes. Nothing in the spec would have caught `StreamOpenResponse`, and
   §8.3's replacement message — `MSG_STREAM_OPENED { period, rates, .. }`,
   annotated "plain data, no ids" — **is still a `#[repr(C)]` struct sent by
   `ipc::send`**. The leak survives the migration verbatim unless F1 and F13 land
   independently of it.
2. **Untrusted deserialization (F2) is a different boundary.** §4.5 gives a
   careful policy for a bad *handle*; `recv_payload<T: Copy>` is bad *data*, one
   trust boundary further out. The spec's threat model stops at the syscall.
3. **Daemon-internal namespaces (F7) are outside §7's table.** `TcpSocketId` is
   the same class as `PipeId` and is a live cross-process authority hole today.
   The spec supplies the mechanism and assigns no obligation; no stage touches
   netd. **Recommend a short section: a daemon's object ids are the same class,
   and handle transfer is what retires them** — with `toyos/src/net.rs`'s
   one-connection-per-request shape named as the thing that has to change first.
4. **§10 is the least specified part of the spec relative to the code it
   governs.** Twelve lines defining `OwnedHandle` and a list of typed wrappers,
   against 50 measured `.fd()` call sites. It needs a `BorrowedHandle` and an
   explicit rule that typed wrappers expose no owned-raw accessor; without that,
   §12.1 invariant 12 ("handle leak/double-close in userland — compile-time
   impossible") is not delivered by the design as written.
5. **§4.1's `RawHandle(pub u32)`** repeats `Fd(pub i32)`'s public field. Private
   field with `raw()`/`from_raw` — otherwise the ABI layer re-creates the thing
   the spec exists to replace.
6. **Length-with-mapping (F8).** §11's `SYS_SHM_MAP(h) -> vaddr` returns a bare
   address; the length still arrives separately and unchecked.
   `SYS_SHM_MAP(h) -> (vaddr, len)` costs nothing and closes it.
7. **`MmapProt` (F5) is not a rights question.** `Rights::MAP` gates whether you
   may map; it says nothing about the page protection of the resulting mapping,
   which is what the kernel currently discards.
8. **A number the spec should carry and does not: handle-space exhaustion is
   uptime-dependent.** §4.1 gives 12 slot bits and 20 generation bits; §12.3
   item 4 says "generation wrap → slot permanently retired; whole-table
   retirement → panic", and §13 repeats it without a figure. **Computed: 4096
   slots × 2^20 generations = 4,294,967,296 handle installs before every slot is
   retired.** At 100 installs/second that is 497 days; at 1000/second, 49.7 days.
   For a compositor meant to run for months this is a reachable kernel panic with
   no reclamation path in the design. The spec should either state the number and
   accept it, or say how a retired slot comes back.

### One place a simpler design would read better

**§4.2's `Rights::READ` and `Rights::WRITE` are partly redundant with §6.1.**
Once pipes are two distinct end types, "write to a read end" is a compile error
and the `WRITE` bit adds nothing for pipes; it still earns its place on
`FileObject`, and `SharedMem` is covered by `MAP`. The spec's own rule is "add a
right when a caller exists" — worth re-checking that list against the object
types once §6 is written, because two bits that only one variant consults are
two bits every dispatch site has to reason about. Not a defect; a place where the
design should be re-measured against its own rule after §6 lands.

---

## Sequencing

Ordered by dependency and by fork window, not by size.

1. **F13** — layout assertions. Zero API change, zero fork impact, and it is what
   makes F1 non-recurring. Nothing depends on it, everything benefits.
2. **F1 + F2** — one trait move, one bound, one `_pad0`, one `MAX_FRAME_LEN`.
   No wire change, no fork impact, no call-site change.
3. **F6, F15, F12** — kernel-local and bootloader-local, no fork impact. F6 is
   the one that deletes runtime checks.
4. **F3 + F4 + F9** — one quiet-tree fork window covering mio (`waker.rs:13`) and
   std (`tls.rs:30`, `process/toyos.rs:401`). Three unchecked wrappers, one
   window.
5. **F10, F11** — bootloader, no fork impact, independent of everything.
6. **F5, F16** — owner decisions first (narrowing the ABI; changing four return
   conventions), then whichever option is chosen.
7. **F7, F8, F14** — the three that belong inside the capability-handles
   migration, and that the spec should absorb the corrections above before
   starting. F7 in particular deletes the most code of anything in this audit and
   should not be done twice.
