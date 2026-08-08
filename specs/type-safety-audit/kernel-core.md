# Type-safety audit — `kernel/` core

Scope: `kernel/src/*.rs`, `kernel/src/mm/`, `kernel/src/sched/`. Excludes
`kernel/src/drivers/` and `kernel/src/arch/`, which another agent owns —
`arch/syscall.rs` and `drivers/nvme.rs` appear only where they are the caller or
callee of something in scope.

Read-only. Nothing was built, nothing was run, no code was changed. Every claim
is off the source at `3e2c975`, with `git show HEAD:<path>` where the working
tree was in doubt. **No finding here has been reproduced on a guest** — where a
finding names an attack, the reproduction is written out so the next agent stages
it rather than believing this file. Every count below came from a command; the
commands are named inline.

## How these are ranked

By (a) the bug class the current shape permits and whether it has bitten this
codebase, and (b) whether the proposed code *reads better* — which for the
findings here almost always means it is **shorter** and has **fewer runtime
checks**, because a check the type makes unnecessary is a check that can no
longer be forgotten at one call site out of five.

Size of the change is reported as fact under **Blast radius**, because sequencing
needs it. It is never an argument. Nothing here is softened, deferred or dropped
because it is large; two findings (§7, §8) are cross-file redesigns and are
recommended on the same footing as the four-line ones.

A finding must still name a bug the current shape permits, *or* be an objective
improvement in how the code reads. "This could be a newtype", with no bug and no
readability gain, is dropped — that rule exists to prevent renaming churn, not to
prevent work, and §Not flagged says which items it dropped and why.

---

## Summary

1. **FIXED.** The kernel did pointer arithmetic on a ring header userland can
   write (`pipe.rs`). Reproduced, then closed by making the kernel stop reading
   the header rather than by validating it — see §1.
2. **`Descriptor` releases four kinds of resource in two hand-copied `match`
   arms, not in `Drop`** (`fd.rs`). `FdTable::insert_at` documents that it
   replaces a live descriptor, and the replaced one leaks everything `fd::close`
   would have released. Consolidating deletes one of the two 21/23-line copies
   and both `_ => {}` arms.
3. **Five device types as bare `u64` constants with three unlinked `_ =>`
   arms** (`device.rs`). Deletes 20 static declarations down to one array and
   removes two five-way matches entirely.
4. **`KernelSlice`/`Mmio` bound with `offset + len`, which can wrap.** Four
   `checked_add`s. Their totality currently depends on `overflow-checks`, which
   `kernel/Cargo.toml` sets for `[profile.dev]` only while `--release` is a real
   CLI flag.
5. **FIXED.** `pipe::open_reader` checked then acted across two lock
   acquisitions and failed into `.expect` — a userland-reachable kernel panic on
   a legitimate race, and the panic stranded `PIPES`. Closed by making an
   un-refcounted `PipeReader` unrepresentable, not by ordering the two calls
   better — see §5.
6. **Six `pub fn init()` are public reset buttons.** Const-init deletes 4
   wrapper functions, 6 `init` functions, and 22 `.expect("not initialized")`
   sites, and closes a use-after-free.
7. **`block.rs` throws away `bcachefs::BlockNum`/`BlockBuf`.** Deletes the
   `count` argument at 11 call sites and the `.raw()` unwrap at the boundary.
8. **`fd.rs` encodes count, would-block and error in one integer.** 22 `to_u64()`
   encodings in `fd.rs` alone.
9. **`FileSystem` errors are `&'static str`**, collapsed to
   `SyscallError::Unknown` at three sites — userland is told "Unknown" for
   failures the filesystem named.

Two framings worth keeping regardless of which land:

- **The tree usually already contains the answer.** `BlockNum`/`BlockBuf` exist
  in `bcachefs` and are unwrapped at the kernel boundary; `irq_ring::IrqSource`
  is the enum `DEVICE_*` should be; `file_cache`'s `Lock<FileCache>` is the
  const-init the other six statics should be; `IrqGuard`, `OrphanCleanup`,
  `IdleProof`, `MappedPages::unmap_from` are the `#[must_use]` discipline
  `claim_teardown` skips; `PipeReader`/`PipeWriter` are the RAII the other five
  `Descriptor` variants should be. Most of these are "stop discarding the type
  you already have".
- **A bound whose totality comes from `overflow-checks` is a bound that depends
  on a build flag.** `LoadedLib::write_at`, `mm::align_2m_checked` and
  `user_ptr::check_user_range` all `checked_add`; `KernelSlice::check` and
  `Mmio::check` do not.

---

## 1. The pipe ring header is untrusted input the kernel treats as a bound — FIXED

Reproduced on a guest, then fixed. `tests/toyos-rust-tests/src/bin/abuse_pipe_ring.rs`
is the gate; the commit carries the before/after output. `Ring` now owns the
base, the capacity and both cursors in kernel memory, and `RingHeader` in the
shared page keeps only `flags`, which the kernel no longer reads either.

Three things this file got wrong, kept because they are the kind of thing a
read-only audit gets wrong:

- **Not `#DE`.** `capacity = 0` is `%` by a runtime zero, which rustc lowers to
  `panic_const_rem_by_zero`. A kernel panic, not a hardware divide error.
- **The consequence is worse than the panic.** The panic fires inside
  `with_pipes`, and a syscall-context panic is *recovered* — the process is
  killed and `PIPES` stays held forever. The reproduction did not crash the
  machine; it wedged every pipe in the system, which is all IPC, and the harness
  reported a timeout. Reasoning from "the guest takes the machine down" would
  have missed that the severity comes from the stranded lock.
- **It is not an SPSC ring shared with userland.** Nothing outside `ring.rs` has
  ever read or written a cursor, and `write_cursor`/`read_cursor`/`capacity` had
  zero readers in the whole tree (netd maps the header only for
  `is_reader_closed`/`is_writer_closed`). So the proposed
  `read_with_capacity(cap, buf) -> Result<_, RingCorrupt>` solved a problem that
  does not exist: with both cursors kernel-owned there is no untrusted input to
  validate, no error to return, and no call site to thread a capacity through.
  Checking who reads a field is one grep, and it changed the design.

What the finding got right, and what to carry: the tree already stated the rule
(`io_uring.rs:163`), and "the answer is already in the tree" was the correct
frame. So was declining to clamp.

---

## 2. `Descriptor` releases its resources in two hand-copied match arms, not in `Drop`

**Location.** `fd.rs:288-308` (`close`), `:314-336` (`close_all`), `:153-157`
(`FdTable::insert_at`), `:168-177` (`check_room`), `:22-34` (`OpenFile::clone`).
The correct pattern, same file: `:60-79` delegating to `pipe.rs:58-82`.

**The shape.** Five variants own something whose release is not a `Drop`:

| variant | owns | released by |
|---|---|---|
| `File(OpenFile)` | a `file_cache` refcount | `file_cache::release` in the match |
| `Listener(ListenerId)` | a registry entry + a name binding | `listener::remove` in the match |
| `IoUring(RingId)` | a ring instance + a 2 MiB shm region | `io_uring::destroy` in the match |
| `Keyboard`/`Mouse`/`Framebuffer`/`Nic`/`Audio` | a device claim | `device::release_descriptor` |
| `PipeRead`/`PipeWrite`/`Socket` | pipe refcounts | **`Drop`** (`pipe.rs:72-82`) — correct |

`OpenFile` makes the asymmetry explicit: a hand-written `Clone` that bumps the
refcount and **no `Drop` that drops it**.

**The bug it permits.**

- `FdTable::insert_at`'s doc is *"Insert at a caller-chosen id, replacing
  whatever is there"*, and `check_room:170-172` explicitly permits it. The
  replaced `Descriptor` is dropped by `HashMap::insert` and none of the release
  code runs. The one caller that can hit a live id — `sys_dup2`
  (`arch/syscall.rs:1496-1502`) — calls `fd::close` first. Nothing in the type
  system says it must, and `insert_at` is `pub`.
- Both arms end in `_ => {}` (`:307`, `:335`). A new resource-owning variant
  compiles clean and leaks. `Descriptor` has grown this way twice already
  (`Listener`, `IoUring`).
- The arms are near-duplicates that must stay in step. `close` calls
  `io_uring::remove_fd`; `close_all` does not. That may be intentional (the
  process is going away) but nothing records it, and a reader cannot distinguish
  an intended divergence from a missed paste.
- `TmpFs::create` returns an existing `FileId` **without** calling
  `file_cache::open` (`tmpfs.rs`, `create`), while `open_file` does — so the
  refcount undershoots on a re-`create`. Harmless today only because tmpfs files
  are non-evictable and `release` at zero keeps their pages
  (`file_cache.rs:126-131`). It is the same defect as the missing `Drop`, from
  the other side: the count is maintained by convention across a trait boundary.

### Both ways

Current — `close` (`fd.rs:288-308`, 21 lines) and `close_all` (`:314-336`,
23 lines) with a near-identical body, counted with `sed -n '288,308p' | wc -l`:

```rust
// fd::close
match &desc {
    Descriptor::File(file) => {
        if file.modified { let _ = vfs.flush_file(&file.path, file.file_id, file.mtime); }
        let last_ref = file_cache::release(file.file_id);
        if last_ref { vfs.close_file(&file.path, file.file_id); }
    }
    Descriptor::Keyboard | Descriptor::Mouse | Descriptor::Framebuffer(_)
    | Descriptor::Nic(_) | Descriptor::Audio { .. } => device::release_descriptor(&desc, pid),
    Descriptor::Listener(id) => listener::remove(*id),
    Descriptor::IoUring(id) => crate::io_uring::destroy(*id),
    _ => {}
}
// fd::close_all — the same eleven lines again, minus io_uring::remove_fd,
// plus a log! on flush failure
```

Proposed — one exhaustive `release`, and both callers become three lines:

```rust
impl Descriptor {
    /// The one place a descriptor's resources are given back.
    /// No `_` arm: a new variant is a compile error here.
    fn release(self, vfs: &mut Vfs, pid: Pid) {
        match self {
            Self::File(f) => f.release(vfs),
            Self::Listener(id) => listener::remove(id),
            Self::IoUring(id) => crate::io_uring::destroy(id),
            Self::Keyboard => device::release(DeviceType::Keyboard, pid),
            Self::Mouse => device::release(DeviceType::Mouse, pid),
            Self::Framebuffer(_) => device::release(DeviceType::Framebuffer, pid),
            Self::Nic(_) => device::release(DeviceType::Nic, pid),
            Self::Audio { .. } => device::release(DeviceType::Audio, pid),
            Self::PipeRead(_) | Self::PipeWrite(_) | Self::TtyRead(_)
            | Self::TtyWrite(_) | Self::Socket { .. } | Self::SerialConsole => {}  // Drop does it
        }
    }
}

pub fn close(table: &mut FdTable, vfs: &mut Vfs, fd: u32, pid: Pid) -> u64 {
    let Some(desc) = table.remove(fd) else { return SyscallError::NotFound.to_u64() };
    let sources = [desc.read_source(), desc.write_source()];
    if sources.iter().any(Option::is_some) { crate::io_uring::remove_fd(fd, &sources); }
    desc.release(vfs, pid);
    0
}

pub fn close_all(table: &mut FdTable, vfs: &mut Vfs, pid: Pid) {
    for (_, desc) in table.drain() { desc.release(vfs, pid); }
}
```

and `insert_at` stops being a trap:

```rust
/// Replacing a live id releases what was there. This is the only insert that
/// can displace a descriptor, so it is the only one that has to.
pub fn insert_at(&mut self, fd: u32, desc: Descriptor, vfs: &mut Vfs, pid: Pid)
    -> Result<(), SyscallError>
{
    self.check_room(Some(fd))?;
    if let Some(old) = self.map.remove(fd) { old.release(vfs, pid); }
    self.map.insert_at(fd, desc);
    Ok(())
}
```

`sys_dup2` then loses its manual `fd::close` call entirely.

The `File` arm gets the RAII it is missing at the same time, which is what fixes
`TmpFs::create`:

```rust
/// Clone bumps, Drop releases — the shape `PipeReader` already has.
pub struct FileRef(FileId);
impl Clone for FileRef { fn clone(&self) -> Self { file_cache::open(self.0); Self(self.0) } }
impl Drop  for FileRef { fn drop(&mut self) { /* release; the vfs close is the caller's */ } }
```

**What it deletes.** One of the two 21/23-line release bodies; both `_ => {}`
arms; `sys_dup2`'s manual close; `OpenFile`'s hand-written `Clone` (derived once
the refcount is a field). It converts "remember to close before replacing" from
a convention into the only thing `insert_at` can do.

**Blast radius.** `fd.rs`; the four `insert_at` call sites
(`arch/syscall.rs:1502`, `loader.rs:231`, and two inside `fd.rs`);
`tmpfs.rs`/`bcachefs_adapter.rs` where `file_cache::open` is called by hand.
`loader.rs:224-235` builds a fresh table, so it can keep a narrower
`insert_new_at` or pass the vfs it already has in scope.

**Relation to filed work.** A true `impl Drop for Descriptor` needs `&mut Vfs`
and a `Pid`, which `Drop` does not have — the same reason
`specs/capability-handles-spec.md` §5 runs `on_zero_handles` off a deferred queue
rather than from `Drop`. This is a staging step toward that spec, not a competitor:
the exhaustive match wants to exist before the next variant is added, and the
capability migration inherits it.

---

## 3. Device types are bare `u64` constants with three unlinked catch-alls

**Location.** `device.rs:6-10` (constants), `:12-16` (five statics), `:48-114`
(`try_claim`, `_ =>` at `:112`), `:119-129` (`is_owner`, `_ =>` at `:126`),
`:132-144` (`release`, `_ =>` at `:139`), `:147-156` (`release_descriptor`,
`_ =>` at `:154`). `grep -c "_OWNER"` returns **20**; `grep -n "_ =>"` returns
**4**; the file is **157** lines.

**The bug it permits.** Adding `DEVICE_TOUCHPAD = 5` and a `try_claim` arm, and
forgetting `release`'s arm, gives a device that can be claimed and never
released — the claim survives process exit, because `release` silently returns
on an unknown type. Not hypothetical for this tree: `specs/issues/isolation/` records that
`device.rs` recorded five owner PIDs and *nothing outside `release` ever read
them* until `device::is_owner` was added — the same class from the other
direction. The metal track is adding device types.

Second: `is_owner`'s `_ => false` fails closed, `release`'s `_ => return` fails
open. Two catch-alls over the same value with opposite polarity, and nothing says
which is which without reading both.

### Both ways

Current — 26 lines for two functions (`sed -n '119,144p' | wc -l`):

```rust
pub fn is_owner(device_type: u64, pid: Pid) -> bool {
    let owner = match device_type {
        DEVICE_KEYBOARD => KEYBOARD_OWNER.lock(),
        DEVICE_MOUSE => MOUSE_OWNER.lock(),
        DEVICE_FRAMEBUFFER => FRAMEBUFFER_OWNER.lock(),
        DEVICE_NIC => NIC_OWNER.lock(),
        DEVICE_AUDIO => AUDIO_OWNER.lock(),
        _ => return false,
    };
    *owner == Some(pid)
}

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

Proposed — 6 lines, no match, no catch-all, and the shape `irq_ring.rs:41-49`
already uses one file away:

```rust
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u64)]
pub enum DeviceType { Keyboard = 0, Mouse = 1, Framebuffer = 2, Nic = 3, Audio = 4 }

impl DeviceType {
    pub const ALL: [Self; 5] = [Self::Keyboard, Self::Mouse, Self::Framebuffer, Self::Nic, Self::Audio];
    pub const COUNT: usize = Self::ALL.len();

    /// The one place a syscall's `u64` becomes a device.
    pub fn from_raw(v: u64) -> Result<Self, ClaimError> {
        match v {
            0 => Ok(Self::Keyboard), 1 => Ok(Self::Mouse), 2 => Ok(Self::Framebuffer),
            3 => Ok(Self::Nic),      4 => Ok(Self::Audio),
            _ => Err(ClaimError::UnknownType),
        }
    }
}

static OWNERS: [Lock<Option<Pid>>; DeviceType::COUNT] = [const { Lock::new(None) }; DeviceType::COUNT];

pub fn is_owner(dev: DeviceType, pid: Pid) -> bool { *OWNERS[dev as usize].lock() == Some(pid) }

pub fn release(dev: DeviceType, pid: Pid) {
    let mut o = OWNERS[dev as usize].lock();
    if *o == Some(pid) { *o = None; }
}
```

`release_descriptor` collapses the same way. `try_claim` keeps five arms (each
does different work) but loses its fallback — the `UnknownType` decision moved
into `from_raw`, where it happens once.

Call site, `arch/syscall.rs` (`sys_open_device`):

```rust
-    match device::try_claim(device_type, pid) {
+    let Ok(dev) = device::DeviceType::from_raw(device_type) else {
+        return SyscallError::InvalidArgument.to_u64();
+    };
+    match device::try_claim(dev, pid) {
```

**What it deletes.** 5 statics → 1 array (`grep -c "_OWNER"`: 20 occurrences of
the static names go to zero). Two five-way matches (26 lines) → two one-liners.
Three of the four `_ =>` arms. The two functions where forgetting an arm costs
something become unable to have one.

**Blast radius.** `device.rs`, plus every `device::is_owner(DEVICE_*, ..)` gate —
`a88e4ee` added them on the GPU present/cursor path, `SYS_AUDIO_SUBMIT`, the NIC
RX/TX path and `SYS_SET_RT_PRIORITY`, all in `arch/syscall.rs`. Constants become
enum variants at each site.

**Relation to filed work.** `specs/capability-handles-spec.md` §6.5 deletes the
owner statics entirely for a claim *object*. This is not that and does not
compete: the enum is what that spec's `DeviceClaim` would key on, and it is worth
having whether or not the spec lands.

**Same file, recorded not filed.** `try_claim`'s framebuffer arm grants
`info.token` in a loop (`:73-78`); if the second grant fails it returns
`GrantFailed` with the first still granted. A partial grant with no rollback. It
matters once grant has a revoke, which `specs/issues/isolation/` keeps open deliberately —
noted so the revoke work knows this call site exists.

---

## 4. `KernelSlice` and `Mmio` bound with an addition that can wrap

**Location.** `mm/region.rs:29` (`subslice`), `:38` (`check`);
`mm/mmio.rs:23` (`subregion`), `:32` (`check`).

```rust
// region.rs:37-40
fn check(&self, offset: usize, len: usize) {
    assert!(offset + len <= self.size, ...);
}
```

**The bug it permits.** An overflowing `offset + len` wraps to a small number,
the assert passes, and `self.base.add(offset)` is a wild kernel pointer for both
`read` and `write`. Whether it wraps or panics is decided by `overflow-checks`,
which `kernel/Cargo.toml:80-83` sets **only for `[profile.dev]`** — its own
comment reads "debug-assertions and overflow-checks stay on: fail-fast beats
speed here", and there is no `[profile.release]` in that file, so a
`cargo run -- --release` build (`src/main.rs:50` parses the flag) turns both off
and this bound becomes decorative.

**Is it reachable today?** Not on the paths I traced, and that is the finding
rather than an argument against it. `elf.rs:1368` does
`lib.image.read::<u64>(rela.r_offset as usize)` on an `r_offset` that came out of
a file, and is safe only because `load_shared_lib` validated every entry 70 lines
earlier at `elf.rs:1293-1297`, with a `checked_add` and a range test. That is
precisely the arrangement CLAUDE.md names as the class that will recur: *"a
policy enforced at one entry point was simply absent at another that reaches the
same machinery."* `KernelSlice` is the machinery; the policy lives in a caller.

**Proposed.**

```rust
 fn check(&self, offset: usize, len: usize) {
-    assert!(offset + len <= self.size,
-        "KernelSlice OOB: offset={:#x} len={} size={:#x}", offset, len, self.size);
+    let end = offset.checked_add(len)
+        .unwrap_or_else(|| panic!("KernelSlice OOB: offset={offset:#x} + len={len} overflows"));
+    assert!(end <= self.size,
+        "KernelSlice OOB: offset={:#x} len={} size={:#x}", offset, len, self.size);
 }
```

Two in `region.rs`, two in `mmio.rs`. No call-site change. It makes the bound
true under both profiles, which is what the three functions that already do it —
`elf.rs:994`, `mm/mod.rs:34-39`, `user_ptr.rs:54-59` — are for.

**Blast radius.** Four functions, one crate, zero API change.

**Relation to filed work.** Extends `specs/issues/design-debt/`'s `KernelSlice::from_raw`
entry. That says the *size* is a caller's assertion; this says that even given a
correct size, the check against it is not total. Both close under "allocators
construct the slice", and the `checked_add` stands alone in the meantime.

---

## 5. `pipe::open_reader` checks then acts across two lock acquisitions — FIXED

Fixed, and the fix went further than what this section proposed. The `acquire`
below still takes an `id` and can still fail, so it can still be called with the
wrong one, and `PipeReader(id)` remains writable anywhere in `pipe.rs`. What
landed puts the handles in a child module whose field `pipe.rs` cannot name, and
`acquire` takes the `&mut Pipe` — no id, no failure mode, and a handle carrying a
different id than the count it bumped is unwritable. `exists` and `creator` are
deleted rather than kept for careful use, and `sys_pipe_open`'s entitlement
became a closure evaluated inside the acquisition instead of a fact read a moment
earlier. Known issues §3 carries the three residuals.

**Location.** `pipe.rs:171-181`, panicking at `:262-268` / `:270-276`.

```rust
pub fn open_reader(id: PipeId) -> Option<PipeReader> {
    if !exists(id) { return None; }        // takes PIPES, releases it
    add_reader(id);                        // takes PIPES again — .expect("pipe not found")
    Some(PipeReader(id))
}
```

**The bug it permits.** The last reader and writer dropping between the two
acquisitions frees the pipe (`close_read`, `:285-288`), and `add_reader`'s
`expect("add_reader: pipe not found")` panics the kernel. Reachable by an
*entitled* caller: `be604ef` gates `SYS_PIPE_OPEN` on the caller having created
the pipe, holding a descriptor for it, or holding a socket to the creator — a
peer in the third category racing the creator's `close` is a legitimate program
losing a race, not an attack. `sys_socket_create` (`arch/syscall.rs:964-965`)
reaches the same pair. This is CLAUDE.md's "an `expect()` on a value that crossed
the trust boundary".

**Both ways.** Current: 4 free functions (`open_reader`, `open_writer`,
`add_reader`, `add_writer`, `exists`) totalling 25 lines, two of which `.expect`.

Proposed — the refcount bump and the handle construction become one operation,
so an un-refcounted `PipeReader` is unrepresentable even inside the module:

```rust
impl PipeReader {
    /// The only constructor.
    fn acquire(pipes: &mut IdMap<PipeId, Pipe>, id: PipeId) -> Option<Self> {
        let pipe = pipes.get_mut(id)?;
        pipe.readers = pipe.readers.checked_add(1)?;
        pipe.header().flags.fetch_and(!RING_READER_CLOSED, Ordering::Release);
        Some(Self(id))
    }
}

pub fn open_reader(id: PipeId) -> Option<PipeReader> {
    with_pipes_mut(|pipes| PipeReader::acquire(pipes, id))
}
```

`create` (`:157-163`) becomes one `with_pipes_mut` holding both `acquire`s;
`Clone` calls `acquire` too. Note `Clone` cannot fail by construction (a live
`PipeReader` proves `readers >= 1` proves the pipe exists), so its remaining
assert becomes a genuine kernel-bug assert instead of an untrusted-input one.

**What it deletes.** `add_reader`/`add_writer` and `exists` as free functions;
both `.expect`s on untrusted input; one of the two lock acquisitions on the
`SYS_PIPE_OPEN` path.

**Blast radius.** `pipe.rs` only.

---

## 6. Six `pub fn init()` are public reset buttons

**Location.** `pipe.rs:135` + `:147-149`, `listener.rs:63` + `:65-70`,
`io_uring.rs:222` + `:224-226`, `vfs.rs:12` + `:14-16`,
`shared_memory.rs:82` + `:90-92`, `process.rs:575` + `:577-579`. Called in order
at `main.rs:398-404`. `grep -rn "not initialized" kernel/src/*.rs | wc -l`
returns **22**, across 7 files (`io_uring` 8, `page_cache` 7, `pipe` 2, `vfs` 2,
`shared_memory` 1, `scheduler` 1, `sync` 1).

**The bug it permits.** The `Option` is not carrying "uninitialized" — boot order
makes use-before-init a kernel bug and a panic is the right answer for that. What
it carries is **"resettable"**, and `init` is `pub`. A second `pipe::init()`
replaces the whole `IdMap<PipeId, Pipe>`, dropping every live `Pipe` — freeing
2 MiB pages still mapped into userland — while every `PipeReader`/`PipeWriter` in
every fd table still names an id. Their `Drop` then reaches `close_read`'s
`.expect("close_read: pipe not found")` and takes the machine down, after the
use-after-free. `listener::init` and `io_uring::init` have the same property;
`vfs::init` throws away every mount.

Nothing calls `init` twice today. Nothing stops it, and none of the six carries a
doc comment saying it must run exactly once.

### Both ways, `pipe.rs:135-149`

Current — 15 lines, two wrapper functions that exist only to unwrap, two
`.expect`s on a hot path (`with_pipes` runs on every pipe read):

```rust
static PIPES: Lock<Option<IdMap<PipeId, Pipe>>> = Lock::new(None);

fn with_pipes<R>(f: impl FnOnce(&IdMap<PipeId, Pipe>) -> R) -> R {
    let guard = PIPES.lock();
    f(guard.as_ref().expect("pipes not initialized"))
}

fn with_pipes_mut<R>(f: impl FnOnce(&mut IdMap<PipeId, Pipe>) -> R) -> R {
    let mut guard = PIPES.lock();
    f(guard.as_mut().expect("pipes not initialized"))
}

pub fn init() {
    *PIPES.lock() = Some(IdMap::new());
}
```

Proposed — 1 line, and the `with_pipes`/`with_pipes_mut` closures at 16 call
sites (`grep -c` returns 18, of which 2 are the definitions) become plain
`PIPES.lock()`:

```rust
static PIPES: Lock<IdMap<PipeId, Pipe>> = Lock::new(IdMap::new());
```

The call sites shrink too:

```rust
-pub fn exists(pipe_id: PipeId) -> bool {
-    with_pipes(|pipes| pipes.get(pipe_id).is_some())
-}
+pub fn exists(pipe_id: PipeId) -> bool {
+    PIPES.lock().get(pipe_id).is_some()
+}
```

The tree already does this in `file_cache.rs:56-66`:

```rust
static FILE_CACHE: Lock<FileCache> = Lock::new(FileCache {
    files: BTreeMap::new(), next_id: 1, cached_pages: 0, max_pages: 0, ...
});
```

and it shows the residual pattern for the part that genuinely cannot be const
(its RAM-derived budget): keep a field `init` fills, and make its unfilled value
loud — `max_pages: 0` plus `assert!(cache.max_pages != 0, "file cache used
before init installed a budget")` (`:366`).

**What it deletes.** 4 unwrap-wrapper functions (`pipe.rs:137`, `:142`,
`shared_memory.rs:85`, `io_uring.rs:404`) and the closure indirection at their
27 call sites (16 + 8 + 3, from `grep -c` minus the definitions); 6 `init`
functions; and the 22 `.expect("not initialized")` sites — every one of which is
a runtime branch on a path that runs per syscall. It also removes a
use-after-free that nothing today can catch.

**Blast radius.** Six files. The one piece needing thought is making
`IdMap::new` a `const fn`; `Vec::new`, `BTreeMap::new` and
`hashbrown::HashMap::with_hasher` are const, so it is a signature change on
`id_map.rs:38-43` plus a `Default`-free hasher for `listener.rs`'s
`HashMap<String, ListenerId>`. Where a const constructor turns out not to be
reachable for a specific inner type, that static keeps its `Option` and gains the
one-line guard:

```rust
pub fn init() {
    let mut g = PIPES.lock();
    assert!(g.is_none(), "pipe::init called twice");
    *g = Some(IdMap::new());
}
```

---

## 7. `block.rs` throws away the `BlockNum`/`BlockBuf` newtypes `bcachefs` already has

**Location.** `block.rs:8-22` (the trait), `page_cache.rs:61`, `:70`
(`raw_block_read`/`raw_block_write`), `bcachefs_adapter.rs:18-30` (where the
types are discarded), `:189`.

**The shape.** `bcachefs/src/block_io.rs:7` defines `pub struct BlockNum(u64)`
and `:38` `pub struct BlockBuf(pub [u8; BLOCK_SIZE])`, with
`fn read_block(&self, block: BlockNum, buf: &mut BlockBuf)`. The kernel adapter
unwraps both:

```rust
// bcachefs_adapter.rs:18-23
fn read_block(&self, block: BlockNum, buf: &mut BlockBuf) {
    let page = cache.read(dev, block.raw());       // -> u64
    buf.as_bytes_mut().copy_from_slice(page);
}
```

and everything below is untyped:

```rust
// block.rs:12-18
/// Read `count` contiguous blocks starting at `lba` into `buf`.
/// `buf.len()` must equal `count as usize * block_size() as usize`.
fn read_blocks(&mut self, lba: u64, count: u32, buf: &mut [u8]);
```

**The bug it permits.** Two unit confusions, both of which this project has paid
for once (CLAUDE.md records an 8 KiB-sector `#DE` and a page-cache index sized
from the wrong quantity):

- **The parameter is named `lba` and carries a 4 KiB filesystem block index.**
  The NVMe driver converts with `sectors_per_block = 4096 / ctrl.sector_size`
  (`specs/issues/kernel/`). Passing a device LBA where a block index is expected is an
  8× error on a 512-byte-sector disk, silently reading or writing the wrong
  place — no panic, no error return.
- **`count` and `buf.len()` must agree and nothing says so.** `page_cache::sync`
  (`:288`) writes `write_blocks(start, count as u32, &buf[..count * 4096])`,
  keeping the two in step by hand. A caller that gets it wrong hands the driver a
  length the buffer does not have.

Counted against `HEAD` rather than the working tree — `git show HEAD:<path> |
grep "read_blocks(\|write_blocks("` over every tracked `kernel/src` file —
there are **5** call sites: `page_cache.rs:64`, `:73`, `:235`, `:288` and
`gpt.rs:221`. At **4** of the 5 the `count` argument is the literal `1`.

> The working tree has 6 more, in an untracked `kernel/src/usb_gate.rs` that is
> another agent's in-progress USB work. They are not counted above and the
> figure will move when that lands. Counting the tree instead of `HEAD` would
> have given 11 and been wrong for anyone reading this against `main` — the
> failure mode `specs/issues/`'s §1 postscript records.

### Both ways

Current, `page_cache.rs:61-74` and `:288`:

```rust
pub fn raw_block_read(block: u64, buf: &mut [u8; 4096]) {
    dev.read_blocks(block, 1, buf);
}
...
dev.write_blocks(start, count as u32, &buf[..count * 4096]);
```

Proposed — the count *is* the buffer length, so it cannot disagree, and the unit
is in the type:

```rust
pub trait BlockDevice: Send {
    fn device_id(&self) -> DeviceId;
    fn block_count(&self) -> u64;
    fn read_blocks(&mut self, start: BlockNum, buf: &mut [BlockBuf]);
    fn write_blocks(&mut self, start: BlockNum, buf: &[BlockBuf]);
    fn flush(&mut self);
}
```

```rust
pub fn raw_block_read(block: BlockNum, buf: &mut BlockBuf) {
    dev.read_blocks(block, core::slice::from_mut(buf));
}
...
dev.write_blocks(start, &buf[..count]);
```

and the adapter stops unwrapping:

```rust
 fn read_block(&self, block: BlockNum, buf: &mut BlockBuf) {
-    let page = cache.read(dev, block.raw());
-    buf.as_bytes_mut().copy_from_slice(page);
+    cache.read_into(dev, block, buf);
 }
```

The word "LBA" then appears in exactly one place in the tree — `drivers/nvme.rs`,
which is the only file that knows the sector size:

```rust
let lba = start.raw() * self.sectors_per_block as u64;
```

**What it deletes.** The `count` parameter at all 5 call sites in `HEAD` (a
literal `1` at 4 of them) and at the 6 more coming with `usb_gate.rs`; the
`* 4096` and `..count * 4096` slicing arithmetic in `page_cache::sync`; the two
`.raw()` unwraps at `bcachefs_adapter.rs:21`/`:28`; and the contract
`block.rs:13` and `:17` currently state twice in prose. The type states it
instead.

**Blast radius.** `block.rs`, `page_cache.rs` (4 sites), `bcachefs_adapter.rs`
(4), `file_backing.rs`, `gpt.rs` (1), the in-flight `usb_gate.rs` (6), and
`drivers/nvme.rs` — which is another agent's file, so this one needs a hand-off.
The
`BlockBuf` slice form also needs `PageCache`'s chunk storage to hand out
`&mut BlockBuf` rather than `&mut [u8]`; `slot_data_mut` (`page_cache.rs:217`)
already slices exactly 4096 bytes, so it is a cast at one site.

**Same class, folded in rather than filed separately.** `file_cache.rs:10`
(`pub type FileId = u64`) and `block.rs:2` (`pub type DeviceId = u32`) are
aliases, not newtypes. The plausible swaps mostly do not typecheck because the
neighbouring parameter is a `u32` page index — except `file_cache::set_size(file_id:
u64, new_size: u64)` (`:293`), where both are `u64` and swapping them compiles.
Three call sites. A `FileId(u64)` newtype falls out of the same pass.

---

## 8. `fd.rs` encodes count, would-block and error in one integer

**Location.** `fd.rs:201` (`open -> u64`), `:280` (`close`), `:342`
(`try_read -> Option<u64>`), `:454` (`try_write`), `:515` (`seek`), `:561`
(`fsync`), `:574` (`ftruncate`), `:616` (`mark_tty`). `grep -c "to_u64()"`
returns **22** in `fd.rs` and **124** in `arch/syscall.rs`.

The sharp instance, `:424-426`:

```rust
if buf.len() < toyos_abi::audio::AudioCompletionRecord::SIZE {
    return Some(SyscallError::InvalidArgument.to_u64());
}
```

**The shape.** `Option<u64>` where `None` means would-block, `Some(n)` means n
bytes, and `Some(SyscallError::X.to_u64())` means error — three outcomes in a
two-state type plus a sentinel range.

**The bug it permits.** Nothing forces a caller to ask which of the three it got
before using the value as a length or adding it to a position. Today the syscall
layer passes the `u64` through so the sentinel survives to userland, which is why
this is ranked here and not higher. The moment a kernel-side caller wants a byte
count — a read-retry loop, a `sendfile`, the io_uring read op
`specs/iouring-blocking-spec.md` implies — it gets an error code silently treated
as a very large length. The audio arm above already mixes the channels: a
*validation* failure returning through the *byte count* path.

### Both ways

Current, `fd.rs:416-431` (the Audio read arm):

```rust
Descriptor::Audio { info, info_read } => {
    if !*info_read {
        let bytes = info.as_bytes();
        let count = buf.len().min(bytes.len());
        buf[..count].copy_from_slice(&bytes[..count]);
        *info_read = true;
        return Some(count as u64);                                  // a count
    }
    if buf.len() < AudioCompletionRecord::SIZE {
        return Some(SyscallError::InvalidArgument.to_u64());         // an error, same type
    }
    let n = crate::audio::drain_completed(buf);
    if n == 0 { None } else { Some(n as u64) }                       // a count, or would-block
}
```

Proposed:

```rust
pub enum IoOutcome { Ready(usize), WouldBlock, Failed(SyscallError) }

Descriptor::Audio { info, info_read } => {
    if !*info_read {
        let bytes = info.as_bytes();
        let count = buf.len().min(bytes.len());
        buf[..count].copy_from_slice(&bytes[..count]);
        *info_read = true;
        return IoOutcome::Ready(count);
    }
    if buf.len() < AudioCompletionRecord::SIZE {
        return IoOutcome::Failed(SyscallError::InvalidArgument);
    }
    match crate::audio::drain_completed(buf) {
        0 => IoOutcome::WouldBlock,
        n => IoOutcome::Ready(n),
    }
}
```

with one `impl From<IoOutcome> for u64` at the syscall boundary doing the
encoding once, and the `open`/`seek`/`fsync`/`ftruncate` family becoming
`Result<_, SyscallError>` handled by `fd_result`, which already exists at
`arch/syscall.rs:1054`.

**What it deletes.** The 22 `to_u64()` calls inside `fd.rs` collapse to one
conversion at the boundary. The `as u64` on every count goes away. Each arm's
three outcomes stop being distinguishable only by knowing the numeric range of
`SyscallError`.

**Blast radius.** `fd.rs`, 14 `fd::` call sites in `arch/syscall.rs`
(`grep -c` over the eleven `fd::` entry points), and
`io_uring.rs:585` (`process_close` casts `fd::close`'s `u64` to `i32` for the
CQE — which is itself a second re-encoding of the same sentinel and disappears
with this). Crosses into `arch/syscall.rs`, so it needs a hand-off.

---

## 9. `FileSystem` errors are `&'static str`, collapsed to `Unknown` at the syscall

**Location.** `vfs.rs:60`, `:66`, `:70`, `:72`, `:74` (trait), `:401`, `:416`,
`:490`, `:534` (`Vfs`). `grep -c "&'static str>" kernel/src/vfs.rs` returns **9**.
Discarded at `fd.rs:218-220`, `:256-259`, `:566-568` — three sites that map the
string to `SyscallError::Unknown` — plus `:291` (`let _ =`) and `:317-319`
(logged).

**The bug it permits.** The filesystem knows why it failed and userland is told
`Unknown`. `BcacheFsAdapter::write_page` returns `"block allocation failed"`
(`bcachefs_adapter.rs:188`), which is `ResourceExhausted`; `TmpFs::rename`
returns `"not found"`, which is `NotFound`. A program cannot distinguish a full
disk from a corrupt one from a missing file, because the distinction is destroyed
at `fd.rs:219`. That is the same defect class as `SYS_READDIR`'s silent
truncation, one layer up: an answer that is wrong in a way the caller cannot see.

It also cannot be matched on. `flush_file`'s `Err` is a string, so `fd::close`
has nothing to branch on and does `let _ =` (`:291`), discarding a write-back
failure on the process-exit path entirely.

**Both ways.** Current:

```rust
fn create(&mut self, name: &str, mtime: u64) -> Result<FileId, &'static str>;
// fd.rs:217-220
let file_id = match vfs.create_file(path, mtime) {
    Ok(id) => id,
    Err(_) => return SyscallError::Unknown.to_u64(),
};
```

Proposed:

```rust
fn create(&mut self, name: &str, mtime: u64) -> Result<FileId, SyscallError>;
// fd.rs
let file_id = vfs.create_file(path, mtime)?;
```

`SyscallError` is already the type `list` returns (`vfs.rs:51`) and already what
every one of these ends up as, so this is making the existing conversion honest
rather than adding a layer.

**What it deletes.** Three `match { Ok(..) => .., Err(_) => return
SyscallError::Unknown.to_u64() }` blocks in `fd.rs` become `?`. The `&'static
str` type disappears from 9 signatures. The strings themselves — currently
constructed and thrown away at three of five call sites — go too.

**Blast radius.** `vfs.rs` (9 signatures), `tmpfs.rs`, `bcachefs_adapter.rs`
(both implement all five methods), `fd.rs` (5 sites), and wherever
`arch/syscall.rs` calls `vfs::create_dir`/`rename`/`create_symlink`.

---

## 10. Two independent `MAX_CPUS = 8`

**Location.** `trace.rs:41` and `sched/mod.rs:17`. `irq_ring.rs:31`,
`scheduler.rs:29`, `sched/driver.rs:47` and `arch/smp.rs:25` all use the `sched`
one; only `trace.rs` has its own.

**The bug it permits.** `scheduler-core-spec` §11 Stage 9 gates on 1–128 CPUs, so
`sched::MAX_CPUS` is going to be raised. Raising it alone leaves `trace.rs:164`'s
`if cpu >= MAX_CPUS { return; }` silently dropping every trace event from CPUs 8
and above — the diagnostic goes quiet at exactly the widths it exists for, with
no error and no log line. `sched/driver.rs:151` asserts `count <= MAX_CPUS`
against the *other* constant, so the machine boots fine.

The reverse is caught: `TRACE_RINGS` is eight literal `TraceRing::new()` entries
(`trace.rs:130-134`), so raising `trace::MAX_CPUS` alone fails to compile.

**Proposed.**

```rust
-pub const MAX_CPUS: usize = 8;
+pub use crate::sched::MAX_CPUS;

-pub static TRACE_RINGS: [TraceRing; MAX_CPUS] = [
-    TraceRing::new(), TraceRing::new(), TraceRing::new(), TraceRing::new(),
-    TraceRing::new(), TraceRing::new(), TraceRing::new(), TraceRing::new(),
-];
+pub static TRACE_RINGS: [TraceRing; MAX_CPUS] = [const { TraceRing::new() }; MAX_CPUS];
```

`TraceRing::new()` is already `const fn`, so the inline-const array works.

**What it deletes.** One duplicated constant, and a hand-written eight-element
array that has to be edited by hand every time the width changes.

**Blast radius.** Three lines, one file.

---

## 11. Hand-maintained `COUNT` next to an enum, with a `transmute` at the sharp end

**Location.** `mm/pmm.rs:18-33` (`Category`, `NUM_CATEGORIES: usize = 12`),
`:99-100`; `irq_ring.rs:41-48` (`IrqSource`, `COUNT: usize = 4`).

```rust
// Safety: i < NUM_CATEGORIES which equals the number of Category variants
let cat = unsafe { core::mem::transmute::<u8, Category>(i as u8) };
```

**The bug it permits.** The safety comment states an equality the compiler does
not check. `Category` has an exhaustive `name()` (`:36-51`), so *adding* a variant
errors there — the common case is caught. *Removing* one without decrementing
`NUM_CATEGORIES` is not, and transmuting an out-of-range discriminant is UB, not a
panic. Adding one and updating `name()` but not `NUM_CATEGORIES` gives
`CATEGORY_STATS[cat as usize]` an out-of-bounds index on the first allocation of
that category — a panic inside `pmm::alloc_page`, which `mm/alloc.rs:9-26`'s
DESIGN RULE says must never panic (it runs under `dlmalloc.lock()`; a panic there
is the wedge `889d611` closed).

`IrqSource::COUNT` has the same shape with no exhaustive match anywhere to catch
the add: a fifth variant indexes `CpuSlots([AtomicU64; 4])` out of bounds inside
`isr_publish` — from an ISR.

**Proposed.**

```rust
impl Category {
    const ALL: [Self; 12] = [Self::KernelHeap, Self::DemandPage, /* ... */ Self::InitTls];
    const COUNT: usize = Self::ALL.len();
    fn name(self) -> &'static str { match self { /* stays exhaustive */ } }
}

-for i in 0..NUM_CATEGORIES {
-    let cat = unsafe { core::mem::transmute::<u8, Category>(i as u8) };
+for cat in Category::ALL {
+    let i = cat as usize;
```

**What it deletes.** The `transmute` and its safety comment; the free-standing
`NUM_CATEGORIES`; the `i as u8` round trip. A variant added to the enum but not
to `ALL` becomes a length mismatch at compile time. Same three lines for
`IrqSource`.

**Blast radius.** Two files, no API change.

---

## 12. `mm/alloc.rs`: an integer phase machine whose default arm is "ready"

**Location.** `mm/alloc.rs:70-88` (`phase: AtomicU8` + three consts), `:91-122`
(the match), `:139-140` (`static mut EARLY_BUF` / `static mut EARLY_POS`).

**The bug it permits.**

```rust
match self.phase.load(Ordering::Acquire) {
    PHASE_UNINIT => core::ptr::null_mut(),
    PHASE_EARLY  => early_alloc(layout),
    _ => { /* the whole dlmalloc path */ }
}
```

The `_` arm routes any unexpected value into the *live allocator*, which is the
direction CLAUDE.md's fail-fast rule specifically rules out ("exhaustive matches
with panics for unexpected values"). The phase is a `u8` written by two
`pub(super)` functions, so a wrong value is a kernel bug — exactly the case that
should scream rather than proceed into dlmalloc.

`EARLY_POS` is a non-atomic `static mut` read-modify-written in `early_alloc`.
Single-threaded today by boot ordering (`init_early` runs before SMP), with
nothing in the type saying so; Rust 2024's `static_mut_refs` is a hard error for
this shape.

**Proposed.**

```rust
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Phase { Uninit = 0, Early = 1, Ready = 2 }

impl Phase {
    fn load(a: &AtomicU8) -> Self {
        match a.load(Ordering::Acquire) {
            0 => Self::Uninit, 1 => Self::Early, 2 => Self::Ready,
            v => panic!("KernelAllocator: impossible phase {v}"),
        }
    }
}
```

and the `alloc` match becomes exhaustive with no `_`. `EARLY_POS` becomes an
`AtomicUsize` with `fetch_update`, which costs nothing on a path that runs a few
hundred times at boot.

**What it deletes.** Three free-standing `PHASE_*` constants and the `_` arm.

**Blast radius.** One file. The `panic!` sits on the same side of
`dlmalloc.lock()` as the two asserts already at `:96` and `:115`, so it is
consistent with the DESIGN RULE at `:9-26`, which constrains `KernelPageSource`
rather than `KernelAllocator::alloc`.

---

## 13. `Lock::data_ptr` is a safe `pub fn` handing out a lock-bypassing pointer

**Location.** `sync.rs:61-65`.

```rust
/// Raw pointer to the underlying data. Does not acquire the lock.
/// Only for statics that need a stable address for asm (GDT, TSS, IDT).
pub fn data_ptr(&self) -> *mut T { self.data.get() }
```

**The bug it permits.** Any module can obtain `&mut T` aliasing a `Lock`'s
contents, in safe code, while another CPU holds the lock. The restriction to
"statics that need a stable address for asm" is a doc comment. One caller today —
`arch/idt/mod.rs:333`, `IDT.data_ptr() as u64` for the IDTR — which is
legitimate.

**Proposed.** `pub unsafe fn data_ptr(&self)` with a `# Safety` clause (the
caller must not form a reference through it while any guard may exist; for
register loads that take an address only). The single call site gains an
`unsafe {}` block that documents itself, and the doc comment's restriction stops
being advisory.

**Blast radius.** Two lines in `sync.rs`, one `unsafe` block in
`arch/idt/mod.rs` — another agent's file, so it needs a hand-off.

---

## 14. `shared_memory::alloc` open-codes the guard that `align_2m_checked` is

**Location.** `shared_memory.rs:106-110`.

```rust
if size == 0 || (size as usize).checked_add(PAGE_2M as usize - 1).is_none() {
    return Err(Error::InvalidSize);
}
let aligned_size = align_2m(size as usize);
```

**The bug it permits.** Nothing today — the guard is correct and immediately
precedes the call. It is a finding because `align_2m`'s own doc
(`mm/mod.rs:20-22`) says *"Only for a size the kernel computed. Use
`align_2m_checked` for one that came from outside it"*, and this size came from
`sys_alloc_shared`. The safety argument lives two lines above the call instead of
inside it, so a later edit that moves the allocation or adds a second entry point
re-opens it. CLAUDE.md's record of the crafted-`p_vaddr` case is this pattern.

**Proposed.**

```rust
-if size == 0 || (size as usize).checked_add(PAGE_2M as usize - 1).is_none() {
-    return Err(Error::InvalidSize);
-}
-let aligned_size = align_2m(size as usize);
+if size == 0 { return Err(Error::InvalidSize); }
+let aligned_size = align_2m_checked(size as usize).ok_or(Error::InvalidSize)?;
```

**What it deletes.** One open-coded overflow guard, replaced by the function
whose entire purpose is to be it.

**Blast radius.** Three lines.

**A correction while here.** CLAUDE.md and `specs/issues/isolation/` both say
"`mm::align_2m` has no checked form, and four callers take their size from a
device or from userland". `align_2m_checked` **exists** at `mm/mod.rs:34-39`.
`grep -rn "align_2m(" kernel/src/ | grep -v align_2m_checked` returns four
remaining unchecked callers: `shared_memory.rs:110` (this one),
`arch/syscall.rs:1245`, `drivers/panic_console/mod.rs:330`, and
`drivers/xhci/mod.rs:312` — the last of which is deliberate, with the
justification written at `:289-290`. `gop.rs` and `elf.rs`/`loader.rs` already
use the checked form.

---

## 15. `ProcessEntry::claim_teardown` returns a bare `bool`, in the file that invented `OrphanCleanup`

**Location.** `process.rs:378-384`, against `:275-278`.

```rust
#[must_use = "orphaned children must be collected after zombifying a process"]
pub struct OrphanCleanup(Pid);
...
pub fn claim_teardown(&mut self) -> bool { ... }     // no #[must_use]
```

**The bug it permits.** `claim_teardown` is an exclusive claim: exactly one
exit/kill path may tear a process down, and the retire-sweep argument at
`:1016-1039` rests on it. A caller that ignores the `bool` runs teardown on a
process another path is already tearing down.

**Proposed.** Matching the token this file already uses for the weaker obligation
next door, so the precondition becomes a signature rather than a comment:

```rust
#[must_use = "exactly one path may tear a process down"]
pub struct TeardownClaim(Pid);

pub fn claim_teardown(&mut self) -> Option<TeardownClaim> { ... }

fn teardown_bookkeeping(table: &mut ProcessTable, claim: TeardownClaim, code: i32,
                        main_cpu_ns: u64) -> Vec<(Pid, Tid)> {
    let process_pid = claim.0;      // the pid comes from the claim, not from a second argument
    ...
}
```

**What it deletes.** `teardown_bookkeeping`'s `process_pid` parameter and its
`.expect("teardown_bookkeeping: process not found")` (`:918-919`) — the claim
proves the entry exists. One argument and one panic.

**Blast radius.** `process.rs` and its one caller (`exit`, `:1027`, `:1073`).

---

## 16. `UserSafe`'s no-padding obligation is prose

**Location.** `user_ptr.rs:29-47`.

```rust
/// # Safety
/// Must be `#[repr(C)]`, `Copy`, have no padding, and be valid for any bit pattern.
pub unsafe trait UserSafe: Copy {}
```

Impls: `u32`, `u64`, `[u32; 2]`, `[u64; 2]`, `fd::Stat`, `SpawnArgs`,
`RawKeyEvent`, `MouseEvent`.

**The bug it permits.** `copy_out::<T>` writes a kernel-side `T` into user
memory wholesale, so a `T` with padding publishes uninitialised kernel stack.
(When this was written it was `user_mut::<T>` and `user_slice_of_mut::<T>`
handing out a `&mut T`; the copy makes the write more direct, not less.) Not
hypothetical: `specs/issues/audio/` records
`AudioInfo::as_bytes` doing exactly that, fixed at `4fce59c` by *spelling the
padding out as named fields with a `const _` size assert*, so omitting one is an
E0063.

I checked every impl and found **no live instance** — `fd::Stat` is three
`u64`s, the primitives and the arrays are trivially padding-free, and the ABI
structs carry explicit `_pad` fields. The finding is that the trait's third
obligation is the only one nothing mechanises, in a codebase already bitten by it
in a sibling path.

**Proposed.**

```rust
macro_rules! user_safe {
    ($t:ty, $size:expr) => {
        const _: () = assert!(core::mem::size_of::<$t>() == $size,
            concat!(stringify!($t), ": size changed — re-check for padding"));
        unsafe impl UserSafe for $t {}
    };
}

user_safe!(crate::fd::Stat, 24);
user_safe!(toyos_abi::syscall::SpawnArgs, /* measure it */);
```

The assert does not *prove* absence of padding — nothing stable does — but it
turns "someone added a field" from silent into a compile error, which is the
whole of what the `AudioInfo` fix bought.

**Blast radius.** One file, six lines. The sizes must be measured, not guessed.

---

## Examined and deliberately not flagged

Nothing here was dropped for being large. Each was dropped because it names no
bug **and** the proposed code does not read better — usually because it reads
*worse*.

**Already filed — not re-filed. Where I found a new instance it is folded into a
finding above rather than duplicated.**

- `PipeId`, `ListenerId`, `RingId`, `SharedToken` as designations (`specs/issues/`
  §1 THE CLASS, §7; `capability-handles-spec.md` §7 has the disposition for all
  four). `PipeId::from_raw` being `pub` and reachable from `sys_pipe_open` is the
  filed defect.
- `FileBacking` outliving unlink and `NvmeBacking::read_page` re-deriving a block
  from stale extents (`specs/issues/isolation/`, deliberately unassigned pending the
  capability spec). I looked for the same shape elsewhere: `Descriptor::File`'s
  `path` + `file_id` pair goes stale across a `rename`, but the writes still land
  on the right `file_id`, so it is a wart of the same origin rather than a second
  instance.
- `gpu::set_resolution` freeing the framebuffer under its consumers
  (`specs/issues/design-debt/`). The `INFO` overwrite also drops the old `GpuInfo`'s three
  `SharedToken`s with no `unregister` — that is the `SharedToken`-has-no-RAII
  item, not a second one.
- `RingHeader`'s `u32` counters wrapping at 4 GiB (`specs/issues/isolation/`, ASSIGNED).
  §1 above is a different defect in the same struct.
- The io_uring watcher triple (`add_io_uring_watcher` / `remove_io_uring_watcher`
  / `io_uring_watchers`) copied verbatim into `keyboard.rs`, `mouse.rs`,
  `net.rs`, `audio.rs`, `pipe.rs`, `listener.rs`, each paired with a
  `wake_waiters` nothing links it to. This is `specs/issues/kernel/`'s "An io_uring
  `Source` can carry one half of the wake pair", and
  `specs/iouring-blocking-spec.md`'s single `post()` is the designed fix. A trait
  here would move the duplication without closing the pairing, so it would read
  no better — that, not its size, is why it is not a separate finding.
- `SYS_LISTEN` having no namespace, so `listener::listen(name: &str, ..)` is
  stringly-typed by design for now (`specs/issues/isolation/`). `Descriptor::Listener` is
  already a `ListenerId` — `e42532f` closed the survivor I was sent to find.
- `sys_read` blocking on Keyboard and returning `NotFound` on Mouse
  (`specs/issues/kernel/`), with `waitqs::MOUSE`/`NETWORK` having wakes and no waiters.
  No type can decide which of the two answers is right; the entry already names
  that as the open decision.

**Dropped because the proposed shape is not better.**

- **`clock::nanos_since_boot()` returning 0 before `init`** (`clock.rs`), with
  `calibrated()` as an optional guard. Normally exactly what this audit hunts.
  An `Option<Nanos>` return reads worse where it matters: this is called from
  `log!` before the TSC exists, and from the panic path, both of which need it
  total, lock-free and allocation-free. Every call site would gain an
  `unwrap_or(0)`, which is the current behaviour with more code.
- **`POISONED: [AtomicU64; MAX_CPUS]` using `u64::MAX` as "empty"**
  (`scheduler.rs:398`), and `Tid::MAX` / `u32::MAX` as "no thread" at
  `process.rs:1221` and `trace.rs:145-146`. `Option` is not representable in an
  `AtomicU64`, and the colliding value is already the canonical no-thread marker
  everywhere else, so the encoding is consistent rather than accidental. A
  `NonZero` scheme would add a type and delete nothing.
- **`Descriptor::Audio { info_read: bool }`** (`fd.rs:51`, read at `:416-423`).
  Two states, one transition, one reader. An enum is the same number of lines and
  reads the same.
- **`keyboard::ACTIVE_LAYOUT: Lock<usize>`** indexing `LAYOUTS`
  (`keyboard.rs:259-265`). The index can only be produced by `set_layout`'s own
  loop over `LAYOUTS`, so it is in range by construction.
  `Lock<&'static Layout>` is the same length and reads the same.
- **`TmpFs::update_metadata`'s linear scan** for a `FileId`
  (`tmpfs.rs`, `update_metadata`) returning `Ok(())` when it finds nothing. A
  reverse index would be a second map to keep in step; the scan is over a map
  that is small by construction and the silent `Ok` is the honest answer for a
  filesystem where metadata is the namespace entry.

**Not assessable from inside this scope, recorded as a question.**

- **`ProcessData.syscall_counts: [u32; 64]`** indexed by syscall number
  (`process.rs:540`). The indexing site is in `arch/syscall.rs`. **Question for
  the `arch/` owner: is the highest `SYS_*` number below 64, and what happens at
  64?** I did not measure it, and CLAUDE.md forbids writing a number I did not
  run a command for.

**The good half, recorded so a reader knows it was looked at rather than
missed.** `Mmio`, `UserAddr`, `DirectMap`, `Cr3`, `PhysPage`, `UserStack`,
`PointerSource`, `IrqGuard`, `IdleProof`, `OrphanCleanup`,
`MappedPages::unmap_from`, `WatcherGuard`, `Source`'s four exhaustive matches,
`IoUringOp::from_raw`, `trace::record`'s wildcard-free mapping, `FdTable`'s
private inner map, `vfs::MAX_LIST_ENTRIES`' derivation, `irq_ring`'s
index-by-`cpu_id()` API that cannot express a cross-CPU access. These are why
the findings above are as few as they are, and five of them are the templates the
findings propose copying.

---

## What deserves promotion to `specs/issues/`

Not done here — three other agents are writing to that file. In priority order:

1. **§1** (pipe ring header) → §1 "Isolation and untrusted input", a new entry
   under the `pipe.rs` owner beside the existing `RingHeader` 4 GiB entry. The
   only finding here that names a memory-safety hole.
2. **§2** (`Descriptor` release) → §7 "Design debt", cross-referenced from
   `capability-handles-spec.md` §15 as the pre-step it is.
3. **§5** (`open_reader` race into `.expect`) → §1's "Untrusted-input panics that
   remain".
4. **§9** (`&'static str` collapsed to `Unknown`) → §1, beside the `getcwd` and
   `SYS_READDIR` entries: same class, an answer that is wrong in a way the caller
   cannot see.
5. **A correction, not a finding:** §1's "`mm::align_2m` has no checked form" is
   stale. `align_2m_checked` exists; §14 above lists the four remaining unchecked
   callers and which one is deliberate.
