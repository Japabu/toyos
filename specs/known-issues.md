# Known issues

Every open defect, in full. CLAUDE.md carries a one-line summary of each of these
under "Known issues" and points here for the detail; keep the two in step. An
entry leaves this file when the code and `git log` carry the fix — resolved
narrative belongs in a dated investigation doc, not here.

**Swept entry by entry against `ba612c6` (2026-08-04).** Every remaining entry
was re-read against the code it names; where a line number had drifted it was
corrected, and where the defect was gone the entry was deleted with its reason
in that commit's message. Confirmation was by inspection of the named code plus
targeted greps — no boot was taken for this sweep, so anything whose evidence is
a measurement still carries the date it was measured on.

Earlier datings, kept because they say what each figure was taken against:
`a88e4ee` (2026-07-30); §2's panic-path additions and §8's display entry against
`883a84d` (2026-07-31); §8's metal-sim entries against M1 (2026-07-31); §1's and
§3's allocation-sizing entries against `a6935c6` (2026-07-31), from the sweep
that followed the T14's first boot.

---

## 1. Isolation and untrusted input

### THE CLASS: an id or a name treated as a capability

Three separate defects in this file are one defect. A `PipeId`, a service name
and a `SharedToken` are all *designations* — they say which object you mean. None
of them says you are allowed to have it. Where the kernel accepted a designation
as authority, guessing or outliving the designation was the entire attack:

- **`PipeId`** — dense sequential integers, so `for id in 0.. { pipe_open(id, 0) }`
  walked every live pipe. Gated at `be604ef` (below).
- **A service name** — **the instance that motivated this class.**
  `Descriptor::Listener` held the service *name*, and every operation re-resolved
  it through the global registry, so nothing tied a descriptor to the listener it
  was created for. `listen("compositor"); dup(fd); close(fd)` freed the name while
  leaving the dup live: the real compositor's `listen` then *succeeded*, its own
  "already running" check passing, and from that moment the attacker's stale fd
  took connections meant for it. Three calls, no race, no privilege. Closed at
  **`e42532f`** (2026-08-01) by storing a `ListenerId` — never reused, so a
  removed id names nothing forever — with `abuse_listener_hijack.rs` as a real
  exploit test.

  Not closed by `be604ef`, which this file briefly claimed: `Listener(String)`
  is an unchanged *context* line in that commit's own `fd.rs` hunk. See the
  postscript at the end of this section.
- **`SharedToken`** — a bare `u32` with no RAII and no ownership, still open
  (§7).

The adjacent failure, same root: **a reference that outlives the object it
names.** `FileBacking` after an unlink is the live instance (below) — the
reference stays valid-looking while the thing it designates is freed and reused
underneath it. Guessing a designation and outliving one are the two ways a name
gets you something you were never given.

`specs/capability-handles-spec.md` exists to make both unrepresentable: a handle
carries rights, so possession *is* the authority and there is no id left to
guess; and it is a refcount on a kernel object, so the object cannot be freed
while a handle can still reach it. Until then, every new syscall taking a raw id
needs the first question asked, and every cached reference to a filesystem or
device object needs the second. **This is here to predict the next instance, not
to summarise the last four.**

> **Postscript, worth more than the entry above it.** On 2026-08-01 this file
> briefly recorded the listener defect as already closed by `be604ef`, citing a
> doc comment and a type that were sitting in the working tree — the isolation
> agent's fix, written twenty minutes earlier and not yet committed. Read against
> `git show HEAD:kernel/src/fd.rs`, the descriptor still held a `String` and the
> attack still ran.
>
> **In a tree with six agents committing, the working tree is somebody's
> uncommitted opinion. `git show HEAD:<path>` is the arbiter.** A finding has a
> shelf life in *both* directions: it can go stale because the bug got fixed, and
> it can look fixed because someone's work-in-progress is on disk. Both cost a
> wrong conclusion here in one day. Method: `specs/spec-staleness-sweep.md`.

### CLOSED for `/home`, open for `/tmp` — a `FileBacking` outlives deletion of the file it reads

Unlink a file while a process is still demand-paging it and the backing keeps
serving reads.

**`/home` — the information disclosure — is closed.** `NvmeBacking` held
`extents: Vec<Extent>` captured at open and turned a file offset into an
absolute block with no re-validation, so unlink returned those blocks to
bcachefs's `BitmapAllocator`, the next file took them, and the stale backing
read whatever was there. Reproduced from userland with `open`, `rm` and a
write: `byte 0 read through the deleted file's descriptor is 0x5c — the
backing served another file's data`.

`file_backing::FileBlocks` is now the one extent list every backing for a name
reads through, and `BcacheFsAdapter` revokes it wherever the filesystem hands
the blocks back — `delete`, `delete_prefix`, a `rename` over an existing
destination, and the truncating `create`/`create_symlink`. It is keyed by name
rather than `FileId` because `open_backing` — the one a running program's text
lives behind — never opens a file. A read after revocation is `Err`, which the
fault handler already leaves unhandled and `file_cache::read_page` zero-fills.

**Revocation, not lifetime extension**, and deliberately not the capability
refcounting this entry used to ask for: keeping a deleted file's blocks alive
for as long as something can read them is POSIX's answer to a question ToyOS
has not been asked, and doing it honestly still needs
`specs/capability-handles-spec.md`. Making the stale read *fail* is the whole of
what the disclosure needs and it is expressible today.

Gate: `home_backing_revoked`, in the shared boot. It asserts the read is
**zeros**, not merely "not the attacker's byte", so it cannot pass by the blocks
failing to be reused.

**`/tmp` is still open**, and is a correctness wart rather than a disclosure:
`TmpfsBacking::read_page` (`tmpfs.rs:25`) reads through `file_cache` by
`FileId`, and a dropped file's `copy_page_out` fills zeros, so the process
faults in blank pages instead of being told the file is gone. Nothing is
disclosed — tmpfs has no allocator handing the storage to anybody else — so it
wants the same revocation only for honesty, not for safety.

### CLOSED — a `SYS_PIPE_MAP` mapping outlived the page it named

Reproduced, then closed. `SYS_PIPE`, `SYS_PIPE_MAP`, close both fds: the last
`PipeReader`/`PipeWriter` drop freed the 2 MiB ring page back to the PMM with
the caller's writable mapping of it still live, and the next write went into
whatever the PMM had handed that page to. `abuse_pipe_map` is the exploit —
its child maps, closes, writes, and has to die.

The window is now recorded against the pipe (`process::PipeMap`) and `fd::close`
takes it back when the process's last descriptor for that pipe goes; a second
`SYS_PIPE_MAP` for a pipe returns the window the first one made, which is what
bounds the record by the descriptor table. Kept as a note rather than deleted
because the *shape* is still the local one this entry argued against:
`specs/capability-handles-spec.md`'s refcounted objects would make the mapping
itself keep the page alive, and every other cached reference in the kernel still
has the old shape.

### The ring's closed flags are userland's to forge, and netd believes them

The kernel no longer reads `RingHeader::flags`: its own `readers`/`writers`
counts decided every one of the four sites that used to consult them, and the
flag — unlike the count — is in the page `SYS_PIPE_MAP` maps writable.

netd still reads them. `bridge_piped` treats `rx_ring.is_reader_closed()` as "the
client died" and `tx_ring.is_writer_closed()` as "the client stopped writing, so
close the socket"; `cleanup_dead_listeners` aborts a listener's socket on the
same bit (`userland/netd/src/main.rs:1006`, `:1011`, `:1045`). Anyone who can map
one of those pipes can set the bit and make netd tear the connection down.

Today that is the connection's own client, so it is self-harm — but the bound on
*who* is `may_open_pipe`, which is a relationship check and not a capability, and
whose own stated residual is that a peer entitled to one of a creator's pipes is
entitled to all of them. netd's exposure is bounded by that residual, not by
anything netd does.

The general statement, since it is the same one the kernel just had to learn: a
publication is not a channel. netd is reading a value its peer writes and
treating it as a fact about its peer. The kernel's answer was to ask the side
that knows; netd has no such side to ask, which is the actual design gap.

### Process isolation does not hold: what is still ungated

`be604ef` (2026-07-28) closed the headline. `SYS_PIPE_OPEN` now requires that the
caller created the pipe, already holds a descriptor for it, or holds a live
socket to the creator; `SYS_SOCKET_CREATE` must already hold both ends in the
right direction; `SYS_PIPE_MAP` is gated derivatively because it takes an fd
rather than an id. `tests/toyos-rust-tests/src/bin/abuse_pipe_owner.rs` is a real
exploit test — it sweeps ids 0..256 skipping its own, asserts every live foreign
pipe is refused, and asserts non-vacuity so it cannot pass by finding nothing.

**It is a relationship check, not a capability, and it has a stated residual:
a peer entitled to one of a creator's pipes is entitled to all of them.** A
compromised daemon can still walk its peer's other pipes. That is the part to
carry forward now that the alarming sentence is gone — a stopgap's known
remainder is exactly what gets forgotten once the headline is fixed.

Still ungated, in rough order of damage:

- `SYS_LISTEN` — no namespace, so the first process to claim a well-known name
  impersonates that service.
- `SYS_GRANT_SHARED` — **narrowed, not retired: an owner cannot withdraw a grant
  it has made.** Two of the three original clauses are closed by `e7d842f`:
  `grant` is owner-only (`shared_memory.rs:179-181` rejects a non-owner, so a
  grantee cannot re-grant) and the target must name a live process
  (`syscall.rs:1096-1102`). `abuse_shared_grant.rs` and `shm_release_reclaims.rs`
  cover both.

  **The "no revoke" clause survives, and this file briefly retired it by mistake
  — on `release`, which is a different operation.** `release` is a grantee
  dropping *its own* access: `sys_release_shared` passes
  `process::current_process()` (`syscall.rs:1128`) and no syscall lets a caller
  name another pid. `destroy` is owner-only but removes the region for everyone,
  which is not withdrawal of one grantee. **Nothing lets an owner revoke a
  specific grantee**, against its wishes, possibly while mapped.

  Deliberate, and currently sound: with `grant` owner-only, the set that can ever
  map is exactly the set the owner named, so revocation has no caller today.
  `specs/capability-handles-spec.md` §14.5 rejects unmap-others by name, and
  unmapping a running process's pages is a second instance of the
  `gpu::set_resolution` hazard — freeing memory while a consumer may hold
  pointers into it.

  **It stops being sound the moment the reachable set is no longer exactly what
  the owner named** — if delegation or re-grant is reintroduced, or when
  `SYS_HANDLE_SEND` makes a grant transferable. Revisit it then, not before.
- ~~`SYS_SET_KEYBOARD_LAYOUT`~~ — deleted. The kernel has no layout to gate:
  it delivers key transitions and userland translates.

`a88e4ee` gated the GPU present/cursor path, `SYS_AUDIO_SUBMIT`, the NIC
RX/TX path and `SYS_SET_RT_PRIORITY` on `device::is_owner`. Each of the above is
a one-line gate of the same shape, but they need a decision first: which of them
should instead fall out of capability handles
(`specs/capability-handles-spec.md`)? `device.rs` records five owner PIDs and,
until `device::is_owner` was added, nothing outside `release` ever read them —
this is a class, not an instance.

Those gates are exactly as strong as the claim and no stronger. `SYS_OPEN_DEVICE`
is itself first-come and ungated, so a process that beats the daemon to a device,
or claims it after the daemon dies, holds everything the claim unlocks — for
`DEVICE_AUDIO` that includes the RT band, which audio spec §9.4 wants to be a
privilege. "Gated" here does not mean "privileged".

### `SYS_DEBUG` is ungated, and two of its actions are a diagnostic-channel DoS

Action 3 — halt every CPU — no longer exists outside the `test-fatal-halt`
feature. The other three are still reachable by any process at any time, and
the audit that removed action 3 turned up what they cost:

- **0 and 1** (`panic!`, and a null read that faults in kernel context) each
  run a full `crash_report`: dozens of lines into the 64 KiB log ring, a
  `PROCESS_TABLE.try_lock()`, a kernel and a user backtrace with symbol
  resolution, and a `panic_flush` that drains the ring synchronously. A loop
  calling `debug(0)` therefore floods the one channel the kernel reports on and
  spends unbounded time in the panic path, and each iteration takes the
  recovery route, which is documented above as able to strand locks.
- **2** costs one lock permanently, by design, and is one-shot for that reason.

None of this is memory-unsafe and none of it kills the machine. It is a syscall
whose only purpose is to make the kernel misbehave, available to everything —
the same class as `SYS_SHUTDOWN` being ungated, and it wants the same decision:
a capability, or `#[cfg(debug_assertions)]`, or deletion.

### Untrusted-input panics that remain

CLOSED and kept for the residual: **`SYS_READDIR` over a large enough tmpfs
directory** was the cheapest one on this list — `Vfs::list` built a
`Vec<(String, u64)>` with one entry per file and no cap, and its 32,769th
`push` doubled the buffer to 65,536 entries, 2,097,152 bytes, past
`mm::MAX_HEAP_ALLOC`. 1.8 s, `fs::write` in a loop, no privilege. Bounded at
`vfs::MAX_LIST_ENTRIES` (16,384) and refused with `ResourceExhausted`;
`readdir_bound` is the gate.

**The residual is that the bound is on the *mount*, not the directory**, and it
has to be: `FileSystem::list` returns every name in the mount and `Vfs::list`
filters, because there is no per-directory index anywhere in the VFS. So a
tmpfs with 16,385 files cannot list any directory in it, including an empty
one, and every `readdir` is O(mount). The fix for that is a real directory
index, not a bigger constant.

**And `bcachefs` is still unbounded underneath it.** The trait takes the limit
so an implementation can refuse *before* it allocates; `TmpFs` does.
`BcacheFsAdapter` and `ReadOnlyBcacheFsAdapter` check the result instead,
because `bcachefs::Mounted::list` has no count primitive and
`btree::collect_all` materialises every entry first. Their refusal is uniform;
their allocation is not bounded. `/home` is writable by userland, so this is a
live path — still open for the `bcachefs` owner. `Node::parse` no longer reserves
from an on-disk count, but `collect_all` still materialises the whole tree.

CLOSED: **`SYS_SYSINFO`'s per-thread `Vec`** was the same shape one syscall
over — one 24-byte entry per live thread, sorted, so the caller's buffer bounded
what was *written* and nothing bounded what was built. `MAX_SYSINFO_THREADS`
(65,536, derived against `MAX_HEAP_ALLOC`) refuses with `ResourceExhausted`, and
the vector is reserved exactly from the count that decides the refusal, so there
is no growth-by-doubling overshoot left to absorb. The residual is that the
thread count itself is still uncapped: this bounds the syscall, not the machine.

**The gate is the actuator's, not the bound's.** 65,536 threads is 8 GiB of
kernel stacks and no guest can make them, so `test-heap-ceiling` drops the
constant to 16 and `heap_ceiling` spawns threads until the refusal comes (13, on
that boot) and then joins them and checks it recovers. What runs is the shipped
count, comparison and error return; the number is the only thing replaced.

### `SYS_DLOPEN` never dedups and `SYS_DLCLOSE` is a no-op

A process can exhaust its virtual address space by repeated loads of the same
library. The *panic* is closed — `syscall.rs:1435`/`:1446` no longer `.expect` —
but the unbounded VA growth is not, and `SYS_DLCLOSE` (`syscall.rs:298`) still
frees nothing.

Deliberately left by the ELF-hardening pass rather than missed. Dedup is a
semantic change, not a bounds check: a second `dlopen` of a loaded library would
return a handle sharing the first module's id and TLS block, and
`std_tls_dlopen`'s test 10 exercises exactly that case. It needs its own change
with its own test, not a hardening drive-by.

### `bcachefs`: three residual untrusted-input holes and a mount-policy question

Left open deliberately, in the same crate:

1. **`decode_leaf_value` does not range-check an extent.** A file's
   `start_block` comes off the disk unchecked and reaches `read_extents` (via
   `read_link`, which *is* on the adapter) and `NvmeBacking`'s demand paging (via
   `file_extents`). With the child-pointer check removed, a `u64::MAX` block
   number reaches `BlockNum`'s byte-offset multiply and panics with "attempt to
   multiply with overflow" — measured, and the same multiply is what an extent
   reaches today with nothing in the way. `Extent.start_block` is a bare `u64`
   crossing the crate boundary into `kernel/src/file_backing.rs`, which is why.
2. **`read_extents` sizes `vec![0u8; size]` from the on-disk file size.** The
   honest bound is one line — a file cannot be longer than the blocks it names.
3. **`BlockNum::to_byte_offset` multiplies unchecked**, next to a `checked_add`.

And the policy question, for the owner, **not changed here**: `probe()` mounts
any disk whose block 0 carries `BCFS`, version 1, and a CRC32C that checks out.
A CRC is not authentication — whoever writes the image writes the CRC — so the
split is *a token naming this device authorises a format, a checksum anybody can
compute authorises a read-write mount*, and both actions write to the disk.
Detail and a recommendation are below, under "`probe()` mounts on a checksum".

### `probe()` mounts on a checksum, and a stamp over a used volume does not reformat

Two things, from reading `bcachefs_adapter::probe` against the crate:

**The threshold does not match the consequence.** `Storage::Ours` is a
read-write mount: `sync()` rewrites both superblocks, and any file operation
writes the bitmap, btree nodes and data. So mounting a stranger's disk modifies
it, which is a weaker form of the wrong the designation stamp exists to prevent.
Accidental collision is not the risk — random block-0 bytes satisfy 4 bytes of
magic, 4 of version and a 32-bit CRC with probability about 2^-64 — and neither
is a *genuine* upstream bcachefs volume, which does not begin with ASCII `BCFS`
(this crate shares the name and nothing else; §3). The risk is a **deliberately
crafted block 0** on a disk somebody hands you, which is the metal track's
situation exactly.

Recommendation, for the owner to decide:

- **Now, nearly free:** tighten `Superblock::check` from
  `block_count <= device_blocks` to `==`. `format` already writes the device's
  own size, so a volume image copied onto a different disk stops mounting, the
  same property the designation stamp's block count gives a format. It is not
  authentication — an attacker who knows the disk size writes the right number —
  but it costs one character and removes the accidental cases.
- **Then:** close residuals 1–3 above. "Mounting a hostile volume is merely
  rude" is not true while an unchecked extent reaches a block read.
- **The real fix, if the threat model wants one:** read-write requires
  something the attacker cannot compute — a keyed MAC, or a designation-like
  stamp — and everything else mounts read-only. ToyOS has no key store and no
  TPM support, so this is a metal-track decision, not a patch.

**Separately, and reproduced:** a designation stamp written over a disk that
already held a ToyOS volume does **not** cause a reformat. `designate_for_format`
writes block 0 only, `Superblock::read` falls back to the backup superblock at
the last block when block 0 does not parse, and a stamp does not parse — so
`mount()` succeeds from the backup and `probe()` returns `Ours`, mounting the old
volume. Harmless for the harness, which stamps freshly created sparse files, but
it means "re-stamp the disk to reformat `/home`" is not a workflow that works.
`probe`'s doc comment claims the decision comes "from one read of block 0"; it
comes from two, and the second one wins.

### `ftruncate` to a larger size does not persist on `/home`

`set_len(3 MiB)` followed by `metadata().len()` returns the old length. The same
sequence works on `/tmp`, so this is bcachefs-specific.

### Derived allocations: one route demonstrated, one unbounded-but-unstaged, one bound

`b554798`. The class is allocations the loader *derives* from inputs, as opposed
to the ones it reads — a per-input ceiling does not constrain a collection fed
from several of them. Three routes were examined and they are **not** equally
established; recording them as one finding would overstate two of them.

- **Route A — demonstrated and fixed.** Two relocation tables of 87,210 entries,
  each individually accepted by `MAX_HEAP_ALLOC`, feeding one index:
  `GlobalAlloc: dlmalloc asked for 2162688 bytes`. A real panic from real input.
- **Route C (`prescan_relocs`) — genuinely unbounded, fixed, NOT staged.** Its
  inputs are `KernelSlice`s over the loaded image and are never gated by
  `MAX_HEAP_ALLOC` at all, so there is no ceiling anywhere on the path. Staging a
  reproducer needs a multi-MiB `.so` whose millions of entries all pass
  `load_shared_lib`'s validation. **Fixed on reading, not on a reproduction** —
  which is the weakest standard this project accepts, and is recorded as such.
- **Route D (`DT_NEEDED` with no `DT_NULL`) — a bound, not a demonstrated
  defect.** It could not be shown to panic: the input ceiling caps that Vec at
  ~1 MiB, so it stays under. Tightened anyway. Do not let it be cited later as a
  fixed vulnerability.

**The fix shape is better than a bound, and is the reusable part: count by type,
then reserve exactly.** That removes growth-by-doubling overshoot — the actual
trigger — and needs no invented number, so there is nothing to justify or
re-derive later. The only explicit ceiling check left is where two
separately-bounded inputs feed one collection, which is exactly the place a bound
on either input cannot help.

### CLOSED — `RelocationIndex::new()` outlived its callers and is deleted

The unbounded constructor, the shape Route A tripped over, with no caller in the
loader path. Deleted; `with_capacity` is the only way to build one, and it takes
the ceiling check with it. The API change the previous entry could not re-verify
was re-verified by a full suite.

### The bootloader sizes every allocation from a file the ESP handed it

`bootloader/src/main.rs:61-62` reads the UEFI-reported `file_size()` and
allocates that much for the kernel and the initrd, with no bound.
`:103,112` takes `max(p_vaddr + p_memsz)` over the kernel ELF's segments with no
overflow check and allocates it, then `:122` copies `p_filesz` bytes into that
`p_memsz`-sized buffer without checking `filesz <= memsz` — the kernel's own
`elf::parse_layout:419` enforces that, the bootloader does not.

Lower severity than the kernel entries above: it runs before ExitBootServices,
on files we put on the ESP ourselves. It is on the list because the metal track
makes the ESP a thing a user can write to, and because none of the kernel's
protections exist yet at that point.

### ASSIGNED — `std::env::current_dir()` silently returns a wrong path

`getcwd` in `rust/library/std/src/sys/pal/toyos/os.rs:7` passes a fixed
`[u8; 256]`, and `sys_getcwd` copies `min(cwd.len(), buf.len())` and returns that
length with **no error and no signal that it truncated**
(`kernel/src/arch/syscall.rs:736-743`). Any cwd over 256 bytes yields a
truncated path, which the program then builds every other path from.

**A correctness defect, not a path-length limitation.** A refusal would be a
limitation; a wrong answer that looks right is worse, because every consumer
inherits it silently. Found the hard way — it reported 256 bytes for a 2 KiB cwd
and made an agent's test fail against a broken instrument, which is the specific
cost of an instrument that lies rather than refuses.

Fix approved and staged as two halves, and **the kernel half must land first**:
`sys_getcwd` reports the required length instead of claiming success, then std
allocates and retries. Landing the std half alone would have nothing to retry
against.

### ASSIGNED — two syscalls discard a failure signal they already have

`sys_mkdir` calls `vfs.create_dir(&resolved)` and returns `0` unconditionally
(`syscall.rs:1424-1430`). `sys_connect` calls `listener::push_connection(..)`,
which returns `bool`, as a bare statement (`syscall.rs:1042`; `listener.rs:97`).

Filed as one entry because the pattern is the finding, not either instance:
**a bound is only as good as the caller's willingness to hear "no".** In both
cases the underlying operation can already refuse, and the syscall layer throws
the answer away — which is exactly why neither can be given a bound today
without the bound becoming a silent failure.

It is the direct counterpart of the class above. There, a client's request is an
allocation request that needs an owner who can say no. Here the owner *does* say
no and nobody is listening, so adding the cap without fixing the caller would
convert an unbounded resource into a silently dropped request — a worse failure,
because the first is at least visible.

### THE CLASS: a client's request is an allocation request

> **A client's request is an allocation request, and every one of them needs an
> owner who can say no.**

Three instances, and the statement is here because none of the three says it
alone — it is what predicts the fourth:

- the compositor's windows (below),
- netd's piped connections (below),
- `SYS_CONNECT` pinning 4 MiB into an unbounded pending queue.

**The third is worse than it looks, because the attacker does not need to find a
service to abuse — `SYS_LISTEN` is ungated, so it can be its own.** Register a
name, connect to yourself, never accept. No victim required and nothing to
guess.

**The third is closed, and the shape of the close is the reusable part** (read
against `ba612c6`, 2026-08-04). `listener::push_connection` returns
`Result<(), PushError>` (`listener.rs:120`) with a queue depth behind it, and
`sys_connect` (`syscall.rs:1152`) now takes the answer: on `QueueFull` it closes
the client's own fd and returns `ResourceExhausted`, on `NoListener` it returns
`NotFound`. That is the pair this class asks for — a bound *and* a caller that
hears the refusal — and it is why the cap could be added at all. `SYS_LISTEN` is
still ungated, so the attacker can still be its own service; what it gets now is
a bounded number of queued connections and an error.

**And the 4 MiB is gone with it.** A pipe now allocates its 2 MiB ring page on
first use — `pipe::create` is infallible because a pipe with no traffic owns no
physical memory, and `try_write`/`map_page` are where exhaustion is met and
answered. Measured in `abuse_connect_flood`: 32 unaccepted connections cost
**0 KiB**, against the 128 MiB the eager allocation charged for the same
allowance, and the first byte written on one buys **2048 KiB**. So the depth is
now a bound on the queue and not on memory, which is what the entry it guards
was about; `MAX_PENDING_CONNECTIONS` says so in its own comment.

### ASSIGNED — the compositor and netd do not bound what they accept

Neither program has a `MAX_` constant of any kind. The compositor calls
`Poller::new(256)` (`compositor/src/main.rs:747`) and registers three fixed fds
plus one per window, with `windows.push` unguarded (`:127`, `:1377`). netd calls
`Poller::new(64)` (`netd/src/main.rs:1060`) and registers two plus one per tx
pipe. **The 256 and the 64 are guesses, not caps derived from anything.**

This is the same class as the two bounds CLAUDE.md holds up as policy —
`user_ptr::MAX_USER_STR` and `fd.rs`'s `MAX_FDS`, each sized against a stated
ceiling and enforced at the one primitive that can breach it. Nobody wrote these
two. A defect on its own terms, not poller plumbing: the poller capacity is where
it happens to surface first.

**It compounds "No physical memory fairness" below, and the pair is worse than
either alone.** An unbounded window count is a memory-growth path any client can
drive, on a system with no per-process limits, no pressure signal and no OOM
killer. Neither entry is alarming by itself; together a single misbehaving client
takes the machine.

Fix in progress, with two requirements on its shape: the bound must state what it
is a function of, and refusing past it must be an error return — not a panic, and
not a silent drop.

**netd's half is closed, and the two numbers quoted above are both stale.** It
derives `max_piped_connections` from physical memory, caps it at what one poller
can watch, and refuses past it with `ERR_RESOURCE_EXHAUSTED` — gated by
`netd_connection_caps`, which makes the daemon announce the cap and the guest
measure where the refusals start. The `accept` rework added the second bound the
entry did not know it needed, `MAX_PENDING_CONNS`, for connections accepted and
not yet identified; `netd_hostile_peer` measures that one. `Poller::new(64)` is
now `Poller::new(FIXED_POLL_FDS + MAX_PIPED_SLOTS + MAX_PENDING_CONNS)`
(`netd/src/main.rs:1228`). The compositor's half is not this agent's to close and
its numbers here want the same re-reading.

### No physical memory fairness

Any process can allocate unbounded physical memory until the system runs out.
No per-process limits, no memory pressure signals, no OOM killer. A single
misbehaving process starves everything.

### CLOSED — the SDK's IPC framing trusted the peer, and what is left after it

`788decd`. `ipc::send<T: Copy>` published `size_of::<T>()` bytes of the
sender's memory: `StreamOpenResponse` measures 32 against 28 bytes of fields
(rustc `offset_of`), soundd builds it with a struct literal, and bytes 4..8 went
to every audio client. `recv_payload<T: Copy>` asserted on `header.len` off the
wire and transmuted arbitrary bytes into any `Copy` type through `mem::zeroed`.
Bound by `IpcPayload`, which had existed one module away in `net.rs` bounding
only `NetdConn::request`; padding is now a compile error via `ipc_payload!`, and
a malformed frame is `Err`. `MAX_FRAME_LEN` (8192) is the SDK's second `MAX_`
constant — `Poller::MAX_HANDLES` was the first and only.

**Four residuals, and the first is worse than what was fixed.**

1. **CLOSED — the compositor's accept path blocked in `recv_header`, and it
   was not alone.** It called it on a freshly accepted fd and `read_exact` is
   a *blocking* `read`, so a client that connected and sent four bytes parked
   the whole event loop until it disconnected. The survey done to close it
   found three more of the same defect, all reachable by any client:

   - the dispatch read every payload off the fd (`recv_payload`,
     `recv_bytes`, and `skip` behind them), so **a whole header followed by
     silence** did it on an established window as well as on a fresh one;
   - every compositor→client message went out through blocking `send`, so
     **a client that stops reading** fills its 2,097,088-byte pipe and the
     compositor parks in `sys_write` with no deadline. `MSG_GET_RESOLUTION`
     is eight bytes in and sixteen out, so the client drives it;
   - the drain loop ended only when nothing was ready, so **a client that
     always has another frame** kept it from ever reaching `redraw` — a
     freeze with a different shape and the same result.

   The read side is now one non-blocking state machine (`ClientRx`) used by
   pending connections and windows alike, with the whole frame in memory
   before anything acts on it; the write side is `try_send`, whose refusal is
   a `DropReason`; the drain loop has `DRAIN_BUDGET`. Two new bounds,
   `MAX_PENDING_CONNS` (32) and `HANDSHAKE_TIMEOUT` (2 s), and every removal
   prints the pid and why.

   Gated by `metal_sim_compositor_stall`, negative-controlled one revert at a
   time: the pre-fix compositor reds at "connected and silent", a blocking
   `fill` reds at "half a header", a blocking reply reds at "window that will
   not read", and deleting `DRAIN_BUDGET` reds at "composited nothing while
   one client streamed". Two of those cases were **green against the defect
   they name** on the first attempt — the flood probed for liveness before the
   ring was full, and the streamer fed the ring rather than filling it — which
   is the argument for controlling each revert separately rather than once.

   `ipc_hostile_peer` sends whole headers because this used to be open. It
   could take the partial cases now; the stall gate has them instead, because
   it is the one that can also see whether the desktop is still painting.
2. **`Window::set_clipboard` bounds nothing and the compositor reads 116
   bytes.** `window/src/lib.rs:349` sends `text.as_bytes()` with no check,
   while the free `clipboard_set` (`:213`) switches to shared memory above
   4096 — and the compositor keeps `MAX_KEPT_PAYLOAD` (116) of it and
   discards the rest. So clipboard text between 117 and 4096 bytes is silently
   truncated to 116 today, and the `MSG_CLIPBOARD_SET_SHM` doc comment
   (`:45`) names 116 as the threshold the sender does not use. Three numbers,
   one protocol. Past `MAX_FRAME_LEN` the send is now refused rather than
   truncated, which is a different silence, not a fix — `set_clipboard` should
   route through shm like its free-function twin.

   **And the shm route it would move to cannot work in that direction at
   all.** `clipboard_set` allocates the region and sends the token, but never
   grants it: `shared_memory::map` requires membership in `allowed`
   (`kernel/src/shared_memory.rs:227`) and only the owner may add to it
   (`grant`), so the compositor's map of a client's clipboard region is
   `PermissionDenied` by construction. The client cannot fix it either — a
   grant needs the compositor's pid and no syscall tells a client who its
   peer is. So every `window::clipboard_set` above 4096 bytes was a
   *compositor kill* until the map became fallible, and is now a dropped
   client. Both halves of the protocol want the same decision: either the
   receiver allocates and grants (which is what the paste direction does, and
   what its own missing grant has just been fixed to do), or a socket learns
   its peer's pid.
3. **`device::read_info<T: Copy>`** (`toyos/src/device.rs:10`) still builds a
   `T` with `mem::zeroed` and fills it from a read. Lower stakes than
   `recv_payload` — the bytes come from the kernel, not a peer — but it is the
   same shape, and `IpcPayload` is the bound it wants.
4. **CLOSED — netd was the same client-kills-the-daemon shape, and was the
   last daemon carrying it.** It accepted and called `ipc::recv_header` on the
   fresh fd, line for line what the compositor had. The survey done to close
   it found six blocking sites, not one: that `recv_header`; `recv_payload` in
   all eleven handlers, so a whole header followed by silence did the same
   thing one step later; `recv_bytes` for a DNS hostname; `send`/`send_bytes`/
   `signal` for every reply; a blocking `read` of the client's tx pipe in
   `handle_udp_send_to`, waiting for a second write a conforming client never
   makes; and a blocking `write` into the client's rx pipe in the UDP receive
   path. Every one reachable by any client.

   The read side is `ipc::FrameRx<256>` per pending connection, the write side
   is `try_send`, and the two pipe operations are non-blocking with the
   refusal answered rather than waited on. Two new bounds — `MAX_PENDING_CONNS`
   (32) and `HANDSHAKE_TIMEOUT` (2 s) — and every removal prints the pid and
   why. `Client` owns the connection, which deleted a `mem::forget` on the
   accepted fd and eight hand-written `close` calls.

   Gated by `netd_hostile_peer` on `tests/netcase`, negative-controlled three
   ways: the pre-fix daemon reds at "netd did not answer a request in 2s — it
   is parked on a client", deleting the pending cap reds at "netd held all 48
   unidentified connections", deleting the handshake sweep reds at "netd held a
   silent connection for 10.0s without dropping it".

   **Two residuals, both bounded by netd's per-operation model rather than by
   anything netd does.** `TrySendError::Full` is unreachable here: a connection
   carries one reply and is closed, so a client cannot fill its own 2 MiB
   receive pipe — the branch is right and untestable, unlike the compositor's,
   where a long-lived window made it reachable. And the pending cap refuses
   *legitimate* connections while it is full, which is the bound working and is
   why the gate does not assert liveness there; a client that can open 32
   connections can therefore delay every other client's request by up to the
   handshake deadline. That is a fairness question, not a stall, and it wants
   per-process accounting rather than a bigger number.

### OPEN — the T14 desktop froze at 64 s: the class is closed, the instance is not

The owner's machine went dead — no typing, no cursor — about 64 s into a
session, with the kernel log still streaming to the stick for another 9.6 s
until the power went off. That log is what prompted the work above. What it
establishes, and what it cannot:

**Established.** The compositor's 2 s report runs unbroken from 4.5 s to the
batch ending ~64.3 s and then stops, so the compositor stopped compositing
with ~5 reports missed. It did not panic (no backtrace, no `exit: compositor`
— the only panic in the log is toybox `tone`'s cpal `NotFound` at 29.5 s,
which is correct on a machine with no audio driver and 35 s earlier), and it
did not run out of memory (134 of 15404 MB, and the pool table is flat).

**One elimination the log does support on its own.** Every wait in
`Poller::wait` carries `FRAME_INTERVAL`, and the taskbar marks itself dirty
once a second, so a compositor parked in its poller still composites every
second and still reports every two. **It was therefore not in `poller.wait`.**

**Two more from the code rather than the log.** A blocking `write` to the
terminal needs its 2,097,088-byte receive ring full, which is 131,072 unread
messages; a whole session of typing and mouse motion is two orders of
magnitude short. And a blocking `recv_payload`/`recv_bytes` on the terminal's
window connection needs a payload-bearing message, while the only two the
terminal ever sends there — `MSG_PRESENT` and `MSG_DESTROY_WINDOW` — are bare
headers.

**What is left, and why none of it is proven.**

- `recv_header` on a freshly accepted connection needs something to have
  connected at ~64.3 s. The only connect the three surviving processes can
  make is `window::clipboard_set`, which the terminal calls on mouse-up after
  a selection — and the two batches before the freeze are 43 and 30 frames
  against a resting 4–6, with `composite_us_min` at 208 µs against a resting
  32 ms, which is cursor-sized damage: mouse motion over the terminal.
  Consistent, and not proven — `clipboard_set` writes its header into an empty
  2 MiB pipe in the next statement, so the compositor should have been woken.
- `accept` itself, on a listener completion whose queued connection was
  withdrawn. That needs a connector that dies, and nothing spawned or exited
  between `ps` at 50.1 s and the freeze.
- The drain-loop livelock, which needs a client whose fd is permanently
  ready. No producer for that among the three live processes.

**The measurement that would have decided it, and did not exist in time.** A
connection is two 2 MiB pipes allocated at `SYS_CONNECT`, and the PMM dump
counts them: `pipe held=5` at 64.348 s is exactly the compositor↔terminal
socket plus the shell's three tty pipes, so nothing had connected *yet*. The
dumps run every 10–13 s, the next was due around 74–77 s, and the log ends at
73.961. `held=7` would have named the accept path and `held=5` would have
ruled it out.

**What the next boot should capture.** Nothing new, which is the point: the
compositor now names every client it drops and why. A recurrence with no
`compositor: dropping pid` line and no telemetry is a mechanism none of the
four closed ones covers, and that is itself the finding. If it is worth
narrowing further before then, the cheap change is a `pipes=` field on the
compositor's own 2 s report — same cadence as the thing that goes missing,
where the PMM dump's is not.

Do not read the 9.58 s of no kernel output as evidence. It is the longest gap
in the log, but an idle desktop in this same session goes 4–6 s between
kernel lines routinely, and the scheduler lines that produce them come from
idle CPUs rather than from a heartbeat. 1.6× the normal gap is not a signal.

### ASSIGNED — two ABI wrappers return an error word as a value, and a fork blocks each

`syscall::pipe()` and `syscall::tls_alloc_block()` cannot express failures the
kernel already returns. Both fixes are one line of ABI each and both are
**blocked on an edit outside the monorepo**, so the wrappers carry a doc comment
saying they are dishonest until someone has the quiet-tree window.

`pipe()` — `sys_pipe` answers `ResourceExhausted` on three paths (`syscall.rs:835-849`:
no pipe pages, and either `fds.insert` hitting `MAX_FDS`). Computed:
`ResourceExhausted.to_u64() = 0xfffffffffffffff8`, which the wrapper splits into
`read = Fd(-1)`, `write = Fd(-8)`. In-tree that surfaces as a **soundd panic**:
`soundd/src/main.rs:427-428` does `syscall::pipe()` then
`pipe_id(..).expect("pipe_id failed")`, so a client that exhausts the fd table
kills the audio daemon. `net.rs` survives by accident — its next call is
`pipe_id` too, but `map_err`'d. Fix: `pub fn pipe() -> Result<PipeFds, SyscallError>`.
**Fork edit owed:** `mio`, branch `toyos`, `src/sys/toyos/waker.rs:13` —
`let pipe = toyos_abi::syscall::pipe();` becomes `let pipe = toyos_abi::syscall::pipe()
.map_err(|_| io::Error::other("pipe"))?;` (`Waker::new` already returns
`io::Result<Waker>`). Eight other in-tree call sites gain a `?`.

`tls_alloc_block()` — the kernel returns `InvalidArgument` for `module_id == 0`
or a module outside the process's list, and `ResourceExhausted` past
`DTV_INITIAL_CAPACITY` (`arch/syscall.rs:1720-1789`). The doc comment claimed
"Panics in the kernel", which stopped being true at the hardening pass, and
claimed a *physical* address where the kernel returns a **virtual** one — both
corrected in place. Consequence: `__tls_get_addr_slow` adds `offset` to a value
near `u64::MAX` and returns the wrap as a pointer; computed, `InvalidArgument`
plus an offset of 16 is `0xb`. Fix: wrap in `check`.
**std edit owed:** `rust/library/std/src/sys/pal/toyos/tls.rs:29-31` — the
variable is even named `block_phys`. `__tls_get_addr`'s ABI is that it returns
an address and there is no caller to return an error to, so the right answer is
`rtabort!`, which is what the current code is reaching for and constructing the
wrong pointer instead.

Batch them: one quiet-tree window covers both, and the audit's F9 (`get_env`,
`waitpid`) is the same window again.

---

## 2. The panic path

### `screen_early_panic`'s ready marker is published one step before the screen it asserts on

`ready_marker` for that boot is `!!! EARLY PANIC !!!`, and the early branch of
`#[panic_handler]` (`kernel/src/main.rs:142`) does, in this order:

```
log!("!!! EARLY PANIC !!!: {}", info);   // into the ring
drivers::panic_console::capture();
unsafe { drivers::serial::panic_flush() };   // <- the harness stops waiting HERE
drivers::panic_console::render();            // <- the pixels it then asserts on
cpu::halt();
```

So the harness is released by the flush and may take its screendump before
`render()` — a full-screen MMIO blit of an 8x16 text grid — has put a glyph
anywhere. The failure is `"!!! EARLY PANIC !!!" not on screen` with a
**completely empty** decoded screen, which is that and not a rendering defect: a
render that ran and got the wrong glyphs would decode to something.

Measured at HEAD `6abed71`, one session, on a host shared with other agents:
**2 failures in 7 runs** (one inside a full suite, one isolated, five isolated
passes). It is not the concurrent-build window §6 describes — that one reports
as a `panicked at src/build.rs` and has no decoded screen at all — and it is not
the guest dying, which `screendump` reports separately.

The ordering itself is deliberate and should not move: the comment beside it
says the flush goes first so a fault inside the renderer "costs the screen and
never the serial report", which is the right trade on a machine with no
exception handlers yet. What is wrong is the *marker*: it names an event that
precedes the thing under test. The fix is a second line after `render()` for the
harness to wait on, or a screendump that retries until it decodes something.

Noticed while verifying #94's suite runs; nothing in the hotplug path can reach
it, since this boot panics at `main.rs:276` and `xhci::init` is at `main.rs:391`.

### A panic while holding `PROCESS_TABLE` hangs the panicking CPU

`try_recover_from_panic` lands in `sched::driver::idle_loop`, whose
`reap_poisoned` takes that lock unconditionally every iteration, and the dead
thread never releases it. Pre-existing and unchanged by the panic-recovery fix; a
`try_lock` could not have saved it either, since a spinlock's `try_lock` fails
for its own holder too. The general shape — locks a dead thread can strand —
belongs to the capability-handles/ownership work.

**The VFS lock is the same shape**, and it was the one that bit first: a
`read_dir` over 32,769 files panicked inside `vfs::lock()`, and every later
filesystem operation on the machine spun on it. Measured after `889d611` — the
process was killed and the harness still got its end marker, because the test
runner's report path does not touch the VFS. That particular route is bounded
now (§1), but the class is not: any panic under `vfs::lock()` still strands it,
and the allocator was only the worst instance because every context allocates.

### CLOSED — the on-screen console showed only what serial had *not* consumed

Both halves are closed and the second is worth carrying, because it is the
shape rather than the instance. The ring is a history as well as a queue:
`retained` is what is still readable behind `head`, `peek_tail` reads it, and
no drain shortens it — so a panic screen carries context again.

The other half was that a drain into a backend that discards still popped, so
on a machine with no UART and no virtio-console the ring's serial cursor
advanced over a consumer that does not exist. `log_ring::SERIAL_SINK` follows
`serial::has_console()` now, exactly as `FILE_SINK` follows `/log`: a cursor
whose consumer is absent stands still. The load-bearing half was never the
wasted pops but `has_pending()`, which the idle loop's pre-halt check declines
to sleep on — so on the T14 every line logged cost every idle CPU a trip round
the loop to throw the line away, and no drain could ever make the answer no on
merit. Nothing in QEMU can observe the difference: every profile but `--mute`
has a 16550, so the sink is on from phase 1 and the behaviour is byte-identical.

### A machine with no console says nothing between `Boot: complete` and the terminal

**Measured under metal-sim (M1), and worse than "no scrollback".** With
`--metal-sim --mute` and no virtio-console the guest has no output channel at
all once the last boot checkpoint has painted: the failure screen ends at
`Boot: complete`, and soundd's null-sink line and netd's exit line — printed seconds later,
and read directly off the console by `metal_sim_compositor` on the same machine
shape with the 16550 on — reach no pixel and no file. A running ToyOS on the
T14 is mute between `Boot: complete` and the moment the compositor's terminal
exists. That is fine for a first boot and not fine for debugging M2 on the
machine. It is also the entire cost the mute default was buying, which is why
the metal-sim profile now keeps its 16550 by default.

Narrowed but not closed: the last checkpoint now paints where this boot's log
can be read (`main.rs`'s `report_log_destination`), so the panel says whether
there will be anything to go back to. What it cannot give that machine is a
line *after* the checkpoint.

### Nothing distinguishes `panic_console::capture` from a no-op

`capture`/`discard_capture` (`drivers/panic_console/mod.rs:362`, `:374`) have no
test that would fail if they stopped working. Measured, not assumed: with
`capture`'s body replaced by `return`, `screen_late_panic` still passes — and
`main.rs` claimed that test was "the one test that fails if the capture stops
happening". The claim was false; it has been corrected in the code.

An open **testing** gap, not a code defect. The functions were kept for a
narrower surviving reason — freezing the report at the panic instant, where
`live_tail` re-reads a ring that siblings running with IF=0 are still writing
to — and carry a comment saying explicitly not to delete them on the grounds
that the tests pass.

Another gate that cannot fail (`specs/metal-track-history.md`), and the third
found this session, after I5 fairness and the unreachable kernel `check` build.

### A panic while the virtio-console TX queue is wedged *and* unlocked spins

In `submit_and_wait`. Bounding that wait is a `virtio.rs` semantics change that
needs its own discussion.

### CLOSED — a CPU reporting a crash could re-enter the scheduler

Closed centrally at `bd12795`, and **not where this entry pointed.** The entry
proposed tightening the DESIGN RULE to ban `try_lock` and rewriting
`crash_report_panic`'s lookup. The actual property is narrower and belongs
elsewhere: `Lock::try_lock` raises the preempt count, and both its failure path
and its guard's `Drop` lower it — so on the pass that took the count to zero with
`need_resched` set, `preempt::enable` dispatched `do_preempt` **from inside the
crash report**. `preempt::enable` now declines the slow path while
`PerCpu::fault_state` is non-zero (`preempt.rs:129`, `faulting()`), placing "a CPU
inside a fault or panic report is not reschedulable" where preemption is decided
rather than chasing it across four `try_lock` call sites.

`panic_console` had already refused `try_lock` for exactly this reason and
documented it; the rest of the crash path kept using it. That asymmetry — one
module obeying a stricter rule than the file that states the rule — is the shape
worth remembering.

**Caveat: it is not free.** A `fault_state` left non-Normal now costs that CPU its
preemption for the rest of the boot. The invariant holds today (every recovery
path sets Normal, every other path halts), but a leak is now a **hang** rather
than a nuisance. Anyone changing fault handling needs to know that the failure
mode moved.

### No test distinguishes the crash-report preemption fix from a no-op

`bd12795` rests on reading the code, which is the weakest standard this project
accepts. Staging it needs a crash report whose preempt count returns to zero with
`need_resched` set — a timing coincidence the harness cannot ask for. The three
panic-path tests still passing says only that nothing regressed.

Fourth instance this session of the pattern in
`specs/spec-staleness-sweep.md` ("Break it and run it"), and the only one of the
four where the check is genuinely hard rather than merely skipped. Recorded so it
is not mistaken for the same tested standard as the fixes around it.

### `percpu.syscall_rip` is never cleared, so "in syscall context" is a guess

`syscall_entry` stores the user RIP at `gs:[216]` on every SYSCALL and nothing
ever zeroes it. The panic handler's recovery predicate is `syscall_rip() != 0
&& current_tid().is_some()` (`main.rs`), so on any CPU that has ever served a
syscall the first half is permanently true. A panic in IRQ context — a timer
tick, a scheduler assert — with any task current is therefore treated as a
syscall panic: `try_recover_from_panic` poisons that task, kills the process
and rejoins the scheduler.

The consequence is backwards from fail-fast: a kernel bug with nothing to do
with the current process kills an innocent process and lets the machine run on,
instead of halting and reporting. `crash_report_panic` prints a "Syscall:
num=... user_rip=..." block off the same stale value, so the report also names
a syscall that is not running. Clearing it on syscall return is one store; the
honest predicate is a per-CPU "in syscall" depth.

### `fatal_exception`'s `recursive` branch never fires for a nested `#PF`

`page_fault_handler` swaps the fault state to `PageFault` *before* dispatching
(`arch/idt/exceptions.rs:468`), and `fatal_exception`'s `recursive` tests only
`Fatal | Panic` (`:522`). A `#PF` nested inside a panic — the exact case the
short-circuit exists for — is therefore classified non-recursive and runs the
full `crash_report` again.

Termination still holds, through the panic console's `PAINTING` latch and the
per-CPU reentry guard, so this is not a live loop. But `a431e02`'s commit
message credits the `recursive` branch with bounding a renderer fault, and that
mechanism does not fire; the latch is doing all the work. Either widen the test
to include `PageFault`, or stop claiming the branch bounds anything.

### The panic console's memory-type gate checks only the framebuffer's first byte

`kernel/src/drivers/panic_console/mod.rs:292-294`'s `framebuffer_is_reclaimed_ram`,
`maps.iter().find(|e| phys >= e.start && phys < e.end)`, classifies the entry
holding the scanout's first byte and ignores the rest of the range. A firmware
map whose scanout starts in `MemoryMappedIO` but whose tail falls in a
`BootServicesData` entry the PMM later hands out passes the gate, and the
panic-path write lands in the heap — the one outcome the gate exists to make
impossible. Checking every entry overlapping `[phys, phys + size)` is the same
loop. Untestable in QEMU (its map is well-formed); a T14 firmware-map hazard,
so fix it before the first metal boot.

### `capture()` is unlatched, so two simultaneous panics interleave the snapshot

`kernel/src/drivers/panic_console/mod.rs:395-402`. Both panicking CPUs take
`cli` first (`main.rs:102`), so neither takes the other's halt IPI, and both
`peek_tail` into the same static. Harmless in itself — same ring, `len` read
once into a local, so indices stay in bounds — but the design's "exactly one
painter, ever" is true of `render` and not of the buffer it paints from, and
the screen can carry two interleaved reports. The `PAINTING` latch shape
extends to `capture` if this is ever seen.

---

## 3. Kernel correctness and hazards

### `Lock::lock`'s spin is the half of the ticket lock loom cannot reach

`kernel-loom` compiles `kernel/src/sync.rs` a second time against loom's
atomics, so the models drive the real primitive rather than a transliteration.
They drive `try_lock` and `LockGuard::drop`. They do not drive `lock()`: loom
explores a spin as an unbounded branch and gives up — `Model exceeded maximum
number of branches`, which is what the first draft of
`try_lock_observes_the_previous_owners_writes` produced — and the
`loom::thread::yield_now()` that would bound it belongs to loom, not to a kernel
that really does spin.

What that leaves unmodelled is contention on `lock()` itself: the ticket
ordering, and the FIFO fairness the ticket exists to buy. The *release* edge is
shared — both acquire paths end at the guard's `now.fetch_add(1, Release)` — so
the models do exercise the publication side; the waiting side is still certified
by reading.

Nothing in the guest suite can substitute. x86's TSO gives every load acquire
and every store release semantics, so a missing acquire edge in this primitive is
invisible on the only architecture ToyOS currently boots, and becomes observable
on ARM64, which is planned and not built. That is why `try_lock`'s acquire edge
sat on the wrong atomic through every green suite run until a model checker was
pointed at it.

### A scheduler pass may spend two seconds in xHCI before it drains its mailbox

`sched::driver::pass` and `pass_block` both open with `drain_irqs()`, and
`drain_irqs` calls `xhci::poll_if_pending()` — **before** `with_cpu(...)`, and
therefore before the CPU's mailbox drain, its deadline fires and its pick. That
call is not bookkeeping. Its own doc-comment says so:

> it enumerates hot-plugged devices and recovers broken endpoints, and both spin
> on deadlines measured in seconds while holding `XHCI`, which is a ticket
> spinlock and therefore preemption off for its whole life.

The deadline is `xhci::USB_TIMEOUT_NS` = 2 s. `cpu::MAX_PASS_NS`, the budget the
scheduler core asserts against in `feature = "check"` builds, is 200 µs. The two
numbers disagree by four orders of magnitude, and the driver's prologue sits on
the wrong side of the boundary the budget describes.

What a CPU inside that recovery holds is *every message addressed to it*: an
`Adopt` carrying a task, a `Wake` for a parked thread, a `Retire`. Nothing in the
scheduler can shorten it — every reap and every wake is bounded by the owning
CPU's pass latency by design, which is exactly why the design is sound. The one
thing in the tree that notices is `scheduler::retire_task`'s 1 s guard, and it
notices by panicking:

```
retire_task: task not released after 1s: InTransit(CpuId(1))
```

That panic fired on the owner's T14 at 949.792 s of uptime with doom exiting. The
*balance*-path half of it is fixed (spec §7.6.4: `hand_off` reaps a killed task
rather than handing it on, gated by simulator invariant I14). This half is not,
and it would produce the same panic with `Blocked(CpuId(n))` in the message
instead — the guard cannot tell a lost message from a busy CPU, which is what it
is written as if it could.

The second instance of the same shape is the idle loop, which runs
`log_file::poll()` — already recorded in CLAUDE.md as "unbounded and
uninterruptible" — before its `pass()`. On a machine whose log partition is on
the USB stick it booted from, that flush is USB mass-storage I/O on the same 2 s
transfer deadline, and a task adopted onto an idle CPU waits behind it.

Closing this means making xHCI enumeration and endpoint recovery asynchronous, so
that `drain_irqs` only ever does work it can finish: drain the event ring,
dispatch HID reports, note that a port or an endpoint owes work. The debounce and
the port reset were already moved off this path for exactly this reason (CLAUDE.md,
USB hotplug); the control transfers inside `configure` and `recover_endpoints`
were not. Until then, `retire_task`'s bound is measuring the USB bus.

**And the budget cannot see it, twice over.** `cpu::MAX_PASS_NS` is asserted by
`check_pass_duration`, which measures from `SchedPass::begin`'s `now` to the end
of `finish()` — and `drain_irqs()` runs *before* `SchedPass::begin`. The
prologue is outside the window the budget covers, so invariant P would report a
200 µs pass while the CPU had been in the driver for two seconds. Separately,
the assertion is behind `feature = "check"`, whose kernel switch is
`sched-check` (`kernel/Cargo.toml:228`) — and **nothing in `src/` or `tests/`
ever turns it on**, so invariant P has never executed against the kernel in any
image or any test run. Both halves want fixing together: the measured window has
to start where the scheduler entry starts, and the gate has to run somewhere.

### A spawned thread that never runs is invisible: `spawn:` does not record where it was placed

From the T14 field log `boot5-doom-wedge.log` boot 10 (17:41, pre-fix image).
Spawned processes intermittently never execute a first instruction, worsening
over the session: `/bin/ps` pid=17 (69.4 s), `/bin/doom` pid=20 (79.8 s, not
even its banner), `/bin/shutdown` pid=25 (104.7 s). Two earlier dooms
initialized fully, drew their title screen and went silent at exactly `ST_Init`
— where doom starts its further threads — with soundd printing `opening stream`
for pid=11 and never `client 1 connected`. The kernel stayed alive to 114.6 s.

**The `sched:` counters do not indict the scheduler, and cannot.** Two facts
about where that line comes from:

* `parked` is `cpu.parked().count()`, and the only way into `CpuSched.parked`
  is `SchedPass::dispose_block`, which consumes `cpu.running`. **A thread that
  never executed cannot be counted there.** The victims are not in that number.
  What it does count is ordinary long-lived blocked threads — compositor,
  soundd, filepicker, and one per live terminal/shell — so 1 → 2 → 3 as three
  terminal/shell pairs accumulate is the expected reading, not a leak.
* `ready=0 current=None` on every sample is a tautology of the print site:
  `log_health` is called from `idle_loop`, so the CPU printing it is idle by
  construction. CLAUDE.md already says this line is not a heartbeat.

**What the log does isolate is `spawn`.** `driver::spawn` posts its `Msg::Adopt`
with `Urgency::Normal` to `placement()`, and `placement()` picks the CPU with
the lowest *published* load — which on a mostly-idle machine is a **halted**
one. So a spawn is the most delivery-dependent operation in the system: unlike a
wake, which goes to a task's home CPU where other work usually is, a spawn
routinely aims its only reap-or-run event at a CPU that must be interrupted to
see it, and then must complete a whole pass before the thread's first
instruction runs. Everything in §3's first entry — `drain_irqs`'s xHCI recovery
ahead of the mailbox drain, `log_file::poll()` ahead of the idle loop's `pass()`
— sits between that IPI and that first instruction. This machine boots off the
stick it logs to (`usb-storage: disk 0 ready on slot 3, SanDisk Ultra`, and
`9266 bytes of kernel log still on the stick` at the end), so both of those are
USB transfers on `USB_TIMEOUT_NS`.

**It is not the balance-path defect (#142) wearing userland clothes.**
`hand_off`'s kill check fires only for a task whose retire is already claimed; a
freshly spawned thread has no kill bit, so that fix cannot reach this. What the
two share is the *state*: `InTransit` is reachable only by the adopt that
carries it, #142 removed one producer of stuck ones, and `spawn` is the other
producer and is untouched.

**The instrument that is missing.** `loader.rs:1111`'s `spawn:` line records
pid, tid, base, entry, cr3 and five timings, and **not the CPU the task was
placed on**. So no reader of this log can say which CPU owed pid=17 its first
dispatch, and therefore cannot correlate a never-ran spawn against that CPU's
`sched:` lines — which is the one correlation that would separate "the adopt was
never delivered" from "the destination never completed a pass". Fold `dst` into
that line rather than adding a second one; the log ring is itself one of the
conditions that keeps a CPU awake.

### OPEN, ASSIGNED (#142) — a spawned process sometimes never starts, and every stuck terminal in the T14 session is downstream of one

Owner-reported (task #141) and read off two T14 session logs. **It is one
defect, not the two it looked like from the symptoms**, plus a second
independent one in a fork (below). The investigation is the scheduler agent's;
what is here is the evidence and the eliminations, so it is not re-derived.

**What the log establishes.** `/bin/ls` was spawned twelve times. Ten exited
`code=0` in 12–59 ms. Two — pid 10 at 692.459 s and pid 26 at 904.327 s —
**produced no output at all and never exited**, and neither did `/bin/rustc`
pid 18 at 826.991 s. Nothing distinguishes their `spawn:` lines from the
healthy ones: same binary, same `ELF: 3740 relocations indexed`, same
`layout=0ms relocs=0ms deps=0ms tls=1ms total=1ms`. The kernel-side spawn
finished and said so; the process then did nothing, forever.

**Every other symptom is a consequence of that one.** A shell blocked in
`waitpid` on a child that never dies is behaving correctly, and so is a
terminal blocked in `child.wait()` on that shell
(`userland/terminal/src/main.rs:186`). Pairing them off:

| terminal | shell | child it is waiting on |
|---|---|---|
| 5 | 6 | `ls` 10, hung |
| 11 | 12 | `rustc` 18, hung |
| 19 | 20 | `snake` 23, see below |
| 24 | 25 | `ls` 26, hung |
| 27 | 28 | none — **and it is the only pair that exited** |

Shell 28 is the healthy control the same log offers: 95 syscalls, `spawn 3`
paired with `waitpid 3`, and it went on from `ls /bin` to `free` and then
`doom`. So neither a lost exit notification nor a missed wakeup is involved,
and `sys_waitpid` registering on the park lot before it reads the table
(`kernel/src/arch/syscall.rs:1067`) is doing its job.

**Refuted, and it was the first hypothesis because it was a same-day change**
(#129, `85a8433`): a child that takes a surface grab and dies without releasing
it does *not* leave the terminal mute. `surface::Host::poll` clears the grab on
`RxStep::Eof` (`toyos/src/surface.rs:218-224`, `close` at `:322`), the terminal
polls every client fd every pass (`userland/terminal/src/main.rs:73-75`,
`:107`), and **the only program in the tree that grabs is `locale detect`**
(`userland/toybox/src/locale.rs:161`) — neither `ls` nor `snake` does.

**Not reproduced, and here is exactly what was tried** so nobody repeats it. A
guest binary modelled on the chain — a parent owning `tty_piped` stdio and
draining it, a shell role whose stdio is those pipes, spawning `/bin/ls /bin`
with `Stdio::inherit()` and waiting with a 2 s per-child ceiling — ran **120
children in the shared boot (smp=2) and 120 more on a dedicated smp=8 boot,
and every one of them started and exited.** The T14 has eight CPUs, so the
CPU count was the first fidelity gap closed and it was not enough. The chain
*under a live compositor and a real `/bin/terminal`* was the next fidelity step
and was **not** taken: `tests/metalcase`'s initrd carries no terminal, shell or
toybox, and five other tests share that boot.

**The measurement meant to decide it was `ps` — and `ps` is a victim.** The
plan was to read the hung child's state column (`userland/toybox/src/ps.rs:72`)
and split three ways: `R` with no CPU is a task nothing ever picked up, `S`
with no CPU is one that blocked before its first user instruction, and any CPU
at all moves the fault into userland startup. The owner ran it on the T14
during boot 10 of `boot5-doom-wedge.log` and **`ps` pid 17 printed nothing and
never exited**, and neither did `/bin/shutdown` pid 25 or three `doom`s. In
that whole boot the only processes that ever exited were `netd` and one
`locale`.

That is an answer rather than a lost measurement, and it is worth more than the
column would have been: **it strikes a fresh process before it can write a
byte, whatever the program is** — `ls`, `rustc`, `ps`, `shutdown`, `doom` — so
nothing about `ls`'s own I/O path is involved. The kernel logs to the end.

The per-CPU `parked` counters climbing 1 → 2 → 3 is **not** the victims
accumulating, and `ready = 0` on every report is not evidence of anything: a
thread that never ran cannot be in `parked`, and the line is printed from the
idle loop. Both numbers are fixed by where they are printed. The entry above
carries that elimination, and the one instrument that would settle the split
the `ps` column was going to.

Investigation is the scheduler agent's (#142); the shapes are consistent with
one defect. Ctrl+Alt+D remains available and is weaker: `dump_blocked` lists
parked threads on the calling CPU only (`kernel/src/scheduler.rs:559`), so it
can confirm a park but not rule one out — though with `parked` rising on
several CPUs at once it may now be worth taking on whichever CPU services the
i8042.

### OPEN — a `winit` app spins forever when its window is closed, and never exits

Separate from the above, in the fork rather than the tree, and **decided by
reading rather than by a reproduction.** `snake` (pid 23) had a real window —
the compositor reports `windows=2` and 86 frames per 2 s batch from 883.7 s —
which went away at ~896 s, leaving `windows=1`. The process never exited, and
it did not exit when the compositor itself died at 952.8 s either.

`winit-toyos/src/event_loop.rs:483-547` polls the window in an inner loop whose
**only exit is `None`**:

```
loop {
    match win.poll_event(0) {
        Some(Event::Close) => app.window_event(.., CloseRequested),
        ...
        None => break,
    }
}
```

Once the compositor drops the connection the fd is permanently read-ready at
EOF, so `Window::poll_event` reports ready, `recv_event`'s `recv_header` fails,
and it returns `Some(Event::Close)` — **every call, forever**
(`userland/window/src/lib.rs:449-458`, `:443-447`). The loop never yields
`None`, so it never reaches the `exiting()` check at `:570` that `exit()`
inside `CloseRequested` was supposed to trip. The app spins on a core instead
of leaving, which is also why nothing in the log marks the moment.

The fix belongs in the fork (`Japabu/winit`, branch `toyos`): break out of the
poll loop on `Close`, or test `exiting()` inside it. Both other arms of the
same function — the `ControlFlow::Wait` and `WaitUntil` blocking paths at
`:637` and `:707` — decode `Close` the same way and want the same look.

### A syscall runs with interrupts masked, and only incidentally with preemption disabled

`syscall_entry` raises the preempt count before `call {handler}` and lowers it
after, so `preempt::enable`'s `count() == 0` slow path can never fire inside a
syscall — no matter how many locks the handler takes and drops. Spec §7.4 counts
that slow path as an RT-wake safe point and bounds wake latency by "the longest
preempt-disabled section"; in syscall context that section is *the entire
syscall*, and the real bound is the next `kernel_exit_to_user_check`.

The preempt count is the weaker of two independent blockers, and it is not the
one that decides the bound. `MSR_FMASK = 0x40200` (`arch/syscall.rs:57`) clears
IF on every SYSCALL entry, and nothing on the straight-line syscall path sets it
again — the only `sti`s in the kernel are `cpu::enable_interrupts`
(`arch/cpu.rs:113`, reached from `trap_dispatch`'s #PF arm and from init),
`kernel_exit_to_user_check`'s own yield window (`arch/idt/mod.rs:232`), and the
idle loop (`sched/driver.rs:494`). With IF=0 the CPU cannot even be *told* to
reschedule: `KernelHw::need_resched` (`hw.rs:96-108`) documents that a remote
CPU's `need_resched` byte is unreachable from here, so a remote request is only
deliverable as a kick IPI — an interrupt the masked target will not take until
it leaves the syscall.

That makes the entry level's fix ineffective on its own: dropping it around
blocking-capable handler regions cannot move remote RT wake latency at all
while IF stays 0. A fix has to unmask interrupts over those regions — which
means auditing what each one is safe to be interrupted in — or the bound is
accepted and §7.4 corrected.

This was masked until the preempt count was made conserved across a context
switch (§6.4's baselines needed it): before that the count drifted, so a lock
drop inside a syscall reached zero at random and preempted at random. The
behaviour is now deterministic, and deterministically weaker than §7.4 assumes.
Whether that matters is measurable — gate A's wake-lateness distribution is the
instrument — and it did not move at N=8.

### `retire_task` is never reached by `cargo test`

Instrumented across all 140 tests: zero calls. Threads that `join` are removed
from the table (`collect_thread_zombie`), so `process::exit`'s phase-2 retire
sweep finds nothing, and no test kills a live process. Both callers —
multi-threaded teardown with unjoined threads, and `kill_process` — are therefore
untested, including the §7.6 message-plus-park protocol 7b rebuilt them on.

### `handle_retire`'s `need_resched` on a running target is a request the next pass may decline

`preempt_if_due` fires on quantum expiry or an RT task in the band, and on
neither for a merely-killed task — so the pass that `need_resched` asked for can
run, clear the request and resume the task, which then dies only at the real
quantum end. That is what spec §7.6 promises ("bounded by the quantum") and
`retire_task`'s spin deadline is 100 quanta, so it is conformant rather than
broken. Adding `|| current.shared().kill_pending()` to `preempt_if_due` would
make the request mean what it says, for one atomic load per pass.

### A thread retired while parked leaves its node in the wait queue forever

`Msg::Retire` reaps a `BlockedTask` on its home CPU; the `Registration` that
would have dequeued it lives on the dead thread's own stack and is never dropped
(the kernel does not unwind), so the queue keeps an `Arc<TaskShared>` for a task
that is `Dead`. A leak, not a correctness hole — `claim_wake` on a dead task
returns `Claim::Lost` and `wake_one` moves to the next waiter, which is exactly
what spec §8.2's retry arm exists for — but the list grows across process kills
and a `wake_all` walks the corpses. The fix belongs with the intrusive
`wait_node` the core still owes (`waitq.rs` holds waiters in a `VecDeque`
instead; see its module note): with an embedded node, `reap` could unlink it.

### `spawn_thread`'s late failure paths drop a mapped TLS block

The two `return None`s after `PROCESS_TABLE` is taken — the process is gone, or
`tearing_down()` claimed it between phase 1 and there — drop the `ThreadData`
holding a `MappedPages` that is already in the parent's address space. Drop frees
the pages; nothing unmaps the VA. Same shape as the `SYS_TLS_ALLOC_BLOCK`
use-after-free (`fcd481f`), which is why the kernel-stack failure path above them
now calls `MappedPages::release`.

Much narrower: reaching it means losing a race with the target process's own
exit, and the address space is destroyed moments later, so the window is a
sibling thread that has not yet been retired. Not fixed with the rest because
`tls_alloc` is already inside the `Arc<Lock<ThreadData>>` by then and the release
cannot happen under the table lock (it would put `AddressSpace` under
`PROCESS_TABLE`, a lock order this kernel does not otherwise use). Building the
`ThreadData` after the table check, inside the same lock hold, is the shape that
fixes it.

### CLOSED — the decoded input queues are unbounded, so a wedged consumer grows the kernel heap

Both halves are closed by the input-architecture rework. `keyboard::MAX_QUEUED_EVENTS`
and `mouse::MAX_QUEUED_EVENTS` are 512 each, **drop-oldest**: what is worth
keeping when nobody is reading is what was typed most recently, and a queue
that refused new events instead would answer the eventual reader with the
first 512 transitions after it stopped and none of the ones since. Both are
one `pop_front` in the one queueing function each file has, so there is no
push site that can miss the bound. `RawKeyEvent` is two bytes now, so the
keyboard's ceiling is 1 KiB.

The replay half is closed by `device::try_claim`, which calls
`keyboard::discard_queued`/`mouse::discard_queued` the moment the device
changes hands: what was typed while nobody held it belongs to nobody, and a
compositor restarted mid-sentence must not open with the tail of what was
being typed into the one that died. Discarding at *claim* rather than at
release also covers a process that died holding it.

What is still true and not fixed: nothing counts the drops. A counter nothing
reads is dead code, and the diagnostics roadmap's layer 1 is where a real one
belongs.

---

The original entry, kept because the reasoning about the two stages is still
the map of this path:

Found while answering "where do the keystrokes go while the compositor is
wedged", which is the question the T14 freeze (§1) raises and nothing in the
tree had an answer for.

The input path has two stages and only the first is bounded.

**Stage one is fine.** The i8042 ISR shovels raw wire bytes into a 256-byte
lock-free ring (`kernel/src/drivers/i8042/mod.rs:382`), drops the newest on
overflow rather than blocking in interrupt context (`:394-405`), counts the
drops in `DROPPED` (`:113`), and says so and resynchronises when it drains
(`:575-586`). It also cannot overflow *because of* a wedged consumer:
`drain()` runs from `drain_irqs()` at the top of every scheduler pass on
every CPU, whether or not any process holds the keyboard fd.

**Stage two has no bound at all.** `keyboard.rs:8` is
`static KEY_BUF: Lock<VecDeque<RawKeyEvent>>`, and its one producer
(`:115`) is an unconditional `push_back`. Four lines in the tree touch it —
the declaration, the push, `has_data`, `pop_front` — and none of them is a
capacity, an eviction, a drop count or a log line. `mouse.rs:8`'s
`MOUSE_BUF` is the same, with two push sites (`:190`, `:208`); its only
mitigation is that `handle_motion` coalesces a report that changed nothing
(`:185-187`), so a still pointer queues nothing and a moving one queues at
its report rate forever.

So while a consumer is wedged, every keystroke and every pointer report is
decoded and appended to a kernel-heap `VecDeque` that only ever grows.
`RawKeyEvent` is 8 bytes and `MouseEvent` 6, so it is slow in absolute
terms and strictly monotonic. The second half is worse than the growth:
`device::release` does not flush the buffer, so an entire stall's worth of
input is replayed into whatever claims the device next.

**The answer to "where do the keystrokes go" is therefore: nowhere, forever,
uncounted.** Recorded rather than fixed — the bound is policy (what a queue
that nobody is draining is *for*), and the second question every bound owes
is what the producer sees when it is hit, which for an ISR-fed decode path
is a decision about the drop counter's shape and not a one-liner.

### CLOSED — `io_uring::remove_fd` selected pending polls by *fd number*, across processes

A server that polled its listener and then blocked in `accept` with nothing
queued, forever. Found in the layout wizard's gate; the compositor has exactly
the same shape and was one fd number away from the same freeze.

`fd::close` calls `io_uring::remove_fd(fd, &sources)`. The rings it reaches are
found by walking the *source's* watcher list, which is right and is the whole
point: a socket's peer is in another process. It then removed pending polls
whose `fd_num` matched — but `fd_num` is the **closing process's** numbering,
which means nothing in the ring it was applied to. So a client exiting with its
connection on fd 3 posted `-NotFound` for whatever the server had on *its* fd 3.

In the gate that was the surface host's listener: the wizard exits, the host's
listener poll is cancelled with a completion, the host reads that as readiness,
calls `accept` with an empty queue and never returns. It reproduced every run
and looked like a hang in the new IPC rather than a five-year-old line in the
kernel.

Fixed by selecting on the source rather than the fd number: a pending poll is
cancelled iff it is watching one of the sources that is going away. Distinct
`Source` variants for the two directions of one pipe mean the peer's
writability poll is not cancelled by a reader's close, which is what the
fd-number filter accidentally got right for same-process fds and wrong for
everything else.

### A keyboard that resets behind our back is undetectable on the PS/2 wire

The i8042 driver runs the keyboard in set 2 with the controller translating to
set 1, which is Linux's default and the best-trodden EC path. The cost is that
`0xAA` — the keyboard's BAT-complete byte after a self-reset — is bit-identical
to left Shift's break code (`0x2A | 0x80`). `toyos-ps2` therefore does *not*
treat it as a reset; only `0x00`/`0xFF`, the overrun and detection-error codes,
report `Lost` and trigger `keyboard::release_all()`.

Consequence on the T14, where the EC does reset the keyboard after suspend or a
lid event: the reset is not noticed. It is survivable rather than silent
breakage — the keyboard comes back in set 2 with controller translation still
on, so the wire format is unchanged, and the `0xAA` it sends decodes as a Shift
*release*, which is accidentally the right direction for the one state that
could stick. Untested on real hardware. If it does bite, the fix is a
controller-side reconnect probe (`0xF2` identify on a timer), not a wire
heuristic, because no wire heuristic exists.

### `sys_read` blocks on an empty Keyboard fd and returns `NotFound` on an empty Mouse fd

Two fds of the same shape, two different answers to the same question. Pick one.

The spurious-readiness half of this entry is closed: `handle_report` returns the
number of events it queued and `dispatch_report` wakes only on a non-zero count,
so readiness and `has_data()` agree. Userland still reads both fds non-blocking,
which is now belt and braces rather than a workaround.

**The other half, not previously recorded: two of the wait queues are woken by
nobody's benefit.** `sched::waitqs::MOUSE` and `sched::waitqs::NETWORK` each
appear at exactly one site in the kernel — their own wake (`mouse.rs:154`,
`net.rs:54`). Nothing ever parks on either. The wakes are real calls on a hot
path doing nothing, and they are the direct consequence of the asymmetry above:
because `sys_read` returns `NotFound` on an empty Mouse fd rather than blocking,
there is never a parked mouse reader to wake. Fixing the asymmetry by making
Mouse block is what would give `MOUSE` a waiter; deleting the queues is what
would make the current behaviour honest. Do not do neither.

### CLOSED — `bcachefs` operations that undo themselves: what the rename fix did not touch

Five orderings in `bcachefs/src/fs.rs`, found auditing the neighbours of the rename defect
for the same act-before-you-know-what-you-are-acting-on shape. Each is fixed, and each
fix's gate is the probe recorded here, red on the commit before it.

**`Mounted::create` and `create_symlink` deleted before they inserted.** Reproduced on a
64-block volume: `create("keep.bin", 5 blocks)` then `create("keep.bin", 400 blocks)`
returned `Err(NoSpace { requested: 340, available: 0 })` and left `read_file("keep.bin")` as
`NotFound` on an empty volume. Both now go through one `Mounted::put`, which reads the
displaced entry out first, writes, inserts, and frees last — `rename`'s ordering, whose
`retire_displaced` is now shared with it. What it costs: an overwrite holds both files'
blocks at once, so one on a nearly full volume can fail where it used to succeed.
Gate: `a_create_that_runs_out_of_space_leaves_the_old_file_where_it_was`.

**`write_data` leaked every block it had reserved when a later `alloc_up_to` failed.**
Measured: after one failed `create(400 blocks)` on a 64-block volume, **0** further
one-block files fit where an untouched volume takes **60**. The two identical copies
(`Formatted` and `Mounted`) are now one free function that gives the runs back before it
returns. Gate: `a_write_that_runs_out_of_space_gives_back_what_it_took`, which asserts the
baseline 60 first so the comparison cannot be vacuous.

**`delete_by_name` deleted before it verified**, so a key collision destroyed an unrelated
file, leaked its blocks and returned `false`; and the File branch fell through to the
Symlink branch after a non-matching removal, so one call could take two entries out.
`find_by_name` now answers both questions before anything is removed, which also deletes
the duplicated branch. Gate: `a_delete_of_a_name_that_is_not_here_destroys_nothing` — the
collision is *crafted*, by rewriting a stored entry's key to the one another name hashes
to, because ~2^-128 is not a state an insert sequence reaches.

**`delete_prefix` freed the blocks before removing the entry and discarded the removal's
result.** It removes first and frees only what it removed. Gate:
`a_delete_prefix_that_removed_nothing_frees_nothing`, on a crafted tree whose child key
disagrees with the keys beneath it — `collect_all` finds the entry and a descent does not.

**`update_metadata`'s pre-check did not cover every failure it ordered around.**
`check_entry_fits` ruled out `EntryTooLarge` before the delete but not `btree::insert`'s
other rejection, a split with no free block, and that path left the entry deleted and never
put back. The delete is gone entirely: the key is unchanged and `insert` replaces on an
equal key, so it bought nothing. Gate:
`a_metadata_update_that_cannot_be_reinserted_leaves_the_entry_alone`.

A removal that comes back empty for an entry `collect_all` produced is the tree
contradicting itself, and both delete paths now report `CorruptedNode` rather than "not
found".

### ASSIGNED — an empty directory does not stat as a directory: kernel half landed, std half owed

**The kernel half is done and gated.** `Vfs::list` now consults `created_dirs`, so an
empty directory answers `Ok(vec![])` and a path no directory could be answers
`Err(NotFound)` — where both used to be `NotFound` (the entry previously said both were
`Ok(vec![])`; that was wrong in the detail and right in the conclusion, and the code it
named had returned `NotFound` for an empty listing since the root commit).
`empty_dir_stat` is the gate, asserting the distinction at the syscall boundary and
through `fs::read_dir`, with the non-vacuity check that a directory holding a file still
lists. Reverting the `created_dirs` lookup reds it at "an empty directory must list as
empty, not refuse". `fs::read_dir` on an empty directory therefore works now; it used to
be `NotFound`.

**The std half is one line and is not landed.** `sys::fs::toyos::is_dir`
(`rust/library/std/src/sys/fs/toyos.rs:367`) reads a zero-length listing as "not a
directory":

```rust
match syscall::readdir(path_bytes, &mut buf) {
    Ok(n) => n > 0,                                 // <- becomes Ok(_) => true
    Err(SyscallError::ResourceExhausted) => true,
    Err(_) => false,
}
```

With the kernel half landed, `Err(NotFound)` is the only "not a directory" answer, so
`Ok(_) => true` is both correct and complete — a file answers `NotFound` too
(`prefix` is `"foo.txt/"` and nothing lives under it). Until it lands,
`fs::metadata("/tmp/d").is_dir()` is still `false` for an empty `d`, and `cp x d/` still
writes a *file* named `d`. `toybox_file_tools` still puts a file in every directory it
makes for that reason.

**Why it was not landed with the kernel half, which is a process constraint and not a
technical one.** `rust/` is the primary checkout's, and in a linked worktree it is the
empty stub `git worktree add` leaves (`specs/worktrees.md` §2) — so a worktree agent can
neither edit nor build it. The sysroot witness covers `toyos-abi`, `toyos` and
`userland/libc` and *not* std's own sources (§3), so the change would also not be picked
up without `--claim-sysroot`, which rebuilds the shared sysroot and cleans every other
worktree's target directories mid-session. This is the same two-half shape as
`std::env::current_dir()` above and takes the same answer: the kernel half lands first
and is safe alone — `is_dir` returns `false` for an empty directory before and after it,
so nothing regresses and the distinction becomes available. Batch the edit with the other
`rust/` work owed in §1 in one quiet-tree window.

Found in the same file and **not** fixed, for the same reason: `FileAttr::file_type`
(`fs/toyos.rs:88`) answers `is_dir` with `self.file_type == syscall::FileType::Pipe`, and
`stat` at `:507` builds a directory's `FileAttr` with `file_type: FileType::Pipe` to
match. A directory is spelled "pipe" throughout, with a comment excusing it rather than a
type that could not express it. Same window.

### `Command::output()` returns an empty stderr, always

`sys::process::toyos::output` (`rust/library/std/src/sys/process/toyos.rs:235`) reads the
stdout pipe and then returns `Vec::new()` for stderr unconditionally. It has already asked
`spawn` for a stderr pipe, so the bytes exist and are dropped — and a child that writes
more than the pipe holds blocks forever against a reader that never comes.

`Output::stderr` is a documented promise this does not keep, which is the sentinel problem
in another dress: the caller cannot tell "the child said nothing" from "we did not look".
Measured: `/bin/cp` refusing a missing source issues three `SYS_WRITE`s to fd 2 and
`output().stderr` comes back empty.

`wait_with_output()` is the cross-platform path and does read the pipe, so the workaround
is to `spawn()` and call that — which is what `toybox_file_tools` does, one stream at a
time to stay off the two-pipe `read2` path. The fix is for `output` to read both pipes, or
to be deleted so the cross-platform default is used.

### The `bcachefs/` crate does not implement bcachefs — a question for the owner

ToyOS's `bcachefs/` crate implements a ToyOS-native on-disk format written from scratch.
It shares a name with Linux bcachefs and nothing else: ours is `MAGIC = b"BCFS"` plus
`DESIGNATION_MAGIC = b"TOYOS-FORMAT-ME\0"` (`superblock.rs:5,24`) and `NODE_MAGIC = b"BTND"`
(`btree.rs:7`), against upstream's UUID-based `BCHFS_MAGIC` / `BSET_MAGIC ^ sb.uuid` /
`JSET_MAGIC`.

`specs/bcachefs-reference.md` — real research into the *upstream* format — now carries a
warning saying so at the top, because its filename in this repo is a trap. That fixes the
document; it does not fix the collision. A crate that does not implement the format it is
named after is a hazard we keep paying for, in exactly this way. Renaming it is the owner's
call, not something to do in a docs pass.

### `#[alloc_error_handler]` does not exist anywhere in the kernel

Kernel heap exhaustion has no handler. It routes into `try_recover_from_panic`, the path that
frees nothing — so the terminal state of every unbounded-growth entry in this file is an OOM
that cannot report itself cleanly. The three unbounded userland-driven growers under §1 all end
here, which is what makes this worth its own line rather than a clause in each.

### `device-test-strategy` requires a `query-pci` verification that exists nowhere

The strategy's rule is ground truth at the hardware boundary: what QEMU was *told* to create
must be checked against what the guest actually enumerated. No such check exists — no test
queries QMP's `query-pci` and compares it against the guest's view. Every profile's device set
is therefore asserted only by the harness's own construction of the QEMU command line, which is
the same source it would be verifying.

Same class as the three scheduler instruments below: a spec requiring an instrument nobody
built. This one matters most for the metal track, where the whole point is that the machine's
device set is not what the harness chose.

### `boot-image-split.md`'s R2 refactor would fail the suite as written

R2 proposes removing the USB stick from the machine profiles and adding a virtio device. Both
halves break tests that exist today: three machine tests assert on the USB stick, and the
profile it would add a virtio device to is the one whose defining claim is that it has *no*
virtio device — that is what `--metal-sim` is for.

Not a doc bug — a plan defect. A plan that fails the suite is not ready to execute, and the
suite is right here. Whoever picks R2 up must re-scope it against those tests first.

### The scheduler's *per-process* fair split degrades as the machine widens — settled: it is the policy

Worst service spread against the derived bound, in ms, from
`measure fairness_storm:<cpus> 500`:

| CPUs | 1 | 2 | 3 | 4 | 6 | 8 | 12 | 16 | 24 | 32 |
|---|---|---|---|---|---|---|---|---|---|---|
| worst | 30 | 84 | 125 | **198** | **324** | **418** | **634** | 720 | 1056 | 1386 |
| bound | 60 | 108 | 156 | 204 | 300 | 396 | 588 | 780 | 1164 | 1548 |

**Per-process only, and that is a real bound on the defect.** The *per-thread* split does
not degrade with width: measured 10 ms at 1 CPU to 50 ms at 32, against a 60 ms derived
bound — inside its bound at every width — over the same runs where I5 went 30 → 1386. So
threads of a process are shared out fairly among themselves at any machine size; it is the
split *between processes* that widens. The fix has a smaller target than "fairness degrades"
implies.

**Both questions the earlier filing left open are now measured, not argued.**

**Offset, not drift.** Holding the seed count and scaling the storm's per-thread
work: one CPU stays at 30 ms at every window length, while eight go 362 → 602 →
548 ms as the window doubles twice. It saturates rather than accumulating.

**Policy, not model.** Everything deciding who runs next is the shipped core —
`RunQueue`'s insertion-time keys, `FairShare`'s one vruntime pot per process,
`CpuSched::pick`, `answer_steal_requests`' surplus rule. The simulator mocks
time, timer, IPI, halt and switch: the parts that decide *when*, not *who*.

**The mechanism, which is why this is a design consequence and not an
implementation bug.** Every running thread of a process charges one pot, so the
pot advances at the process's *aggregate* rate while each queued thread's key
stays frozen at its insertion. One dispatch of staleness therefore buys more
wall-clock service the more of that process runs at once. That is why it scales
with width, and why careful coding cannot close it — the fix is a policy change.

**Caveat, and it is load-bearing.** These are worst-of-N over adversarially
chosen interleavings, seeded and PCT — not the split hardware would show on an
average schedule. **The mechanism and the scaling are the policy's; the magnitude
is a worst case.** Do not quote these numbers as expected behaviour.

**Connected to §9.2's tie-break, and that is why this is hard.** Threads of a
process sharing one vruntime is *why* the insertion sequence exists
(`specs/scheduler-core-spec.md` §9.2, `queue.rs:18-22`). The degradation here and
sibling starvation are two faces of one decision: **per-process accounting with
per-thread queueing.** Anything that fixes one has to answer for the other.

**But only the per-process face degrades, and that is now measured.** Simulator
invariant I13 measures service per *thread* inside a share over the same
contention windows, narrowed to intervals where every CPU carries the same
number of each member's runnable threads (otherwise the number is placement, not
ordering). From `measure fairness_storm:<cpus>`, against a derived bound of
60 ms at every width — `(rivals + 1) × (QUANTUM + max KernelSection +
2 × RUN_CHUNK)`, five dispatches of one run queue's fair band, with **no lag
term** because a share holds one vruntime and one lag for all its threads:

| CPUs | 1 | 2 | 3 | 4 | 6 | 8 | 12 | 16 | 24 | 32 |
|---|---|---|---|---|---|---|---|---|---|---|
| I13 worst | 10 | 30 | 28 | 28 | 31 | 32 | 35 | 37 | 42 | 50 |
| I5 worst | 30 | 102 | 125 | 198 | 324 | 418 | 634 | 612 | 1046 | 1386 |

Flat where the per-process split runs away. **And the tie-break is not what
keeps it flat** — the pot is charged for every nanosecond any thread of the
share runs, so a re-inserted thread already carries a key strictly above every
sibling queued before it and the band serves them in insertion order whatever
the tie-break is. `(vruntime, TaskKey)` ported literally
(`scenarios::fair_identity_tiebreak`) is invisible to I13, which is why the
negative gate had to be the stronger `fair_identity_within_share`. **The
consequence for the fix**: a redesign replacing per-thread queue keys with an
ordered map of shares each holding a FIFO of its ready threads takes the
ordering job *away* from the pot and hands it to that FIFO, so this face stops
being benign the moment the fix lands. I13 is the gate that says so; it is green
today and its own gate is red on the broken shape, on I13 alone — I5 reports a
perfectly even split while two of three sibling threads never run.

**Entry criteria for the per-share-FIFO redesign.** I5 and I13 together are
close to sufficient and are not sufficient. Three gaps, all prerequisites rather
than follow-ups, and the first is *the* one — the other two are conditions on
trusting the answer, this one is a hole where the answer would be.

1. **The redesign's most novel path has no coverage in the workload class that
   exercises it.** Where a woken thread lands in its share's order falls out of
   the pot today; after the redesign it is decided by the FIFO push, which *is*
   the new code. Nothing measures it. A block drops a thread from I13's measured
   set, so I13's reach inverts exactly against the workloads that would exercise
   it — 96–99% on the fairness storms, where nothing blocks, against
   `crash_md_exit_race` 37%, `rt_wake_latency` 29%, `fork_storm` 9%,
   `futex_storm` 5% and `audio_pipeline` **0%**. **I13 would stay green straight
   through a redesign that got the wake path's ordering wrong**, and it is the
   check that nominally guards fairness. A wake-heavy workload with windows long
   enough to measure does not exist and has to be built first.
2. **I13's reach is a silent casualty of the change it guards.** Its window
   closes when a member's threads stop being evenly spread over the CPUs, and
   the redesign must reimplement `pop_surplus`, which feeds
   `answer_steal_requests` and can therefore change placement — so a redesign
   that disturbs placement makes I13 measure *less* rather than fail, with the
   sweep still printing `clean`. Instrumented rather than left as vigilance:
   `SweepResult::thread_coverage_pct` publishes the fraction of executed time
   I13 had a comparison open for, `invariant_i13_is_measured_and_holds` gates on
   it against 96% / 69% / 99%, and forcing the balance condition false takes it
   to 0% and reds the test. **A/B that number across the redesign; a collapse is
   as loud as a violation.** Named as the third gate-failure shape in
   `specs/spec-staleness-sweep.md`, with the evidence in
   `specs/metal-track-history.md`.
3. **The margin at 32 CPUs is 1.2× and trending up** — 10 ms at one CPU to
   50 ms at 32 against a 60 ms bound — with nothing measured above 32, while
   spec §11 Stage 9 gates on 1–128. Measure 64 and 128 first, or a red at high
   width cannot be attributed to the redesign rather than to the width.
   Compounded by the reach falling with width for an unrelated reason — 55% at
   four CPUs, 45% at eight, because threads exit at slightly different moments
   and unbalance a wide machine sooner. **At the widths Stage 9 targets, I13
   certifies less than half the run**, which is a limit on the invariant and not
   a defect in it.

Found only because I5 measures *service* — nanoseconds actually delivered — rather
than checking vruntime bookkeeping against itself, which would have been true by
construction. The dead-gate lesson from the other side: the first question about a
gate is not whether it passes, but whether it measures the quantity you care
about (`specs/spec-staleness-sweep.md`).

### The scheduler crosses its own derived granularity bound at four of ten widths

Distinct from the entry above, and deliberately not merged with it. That one says
fairness degrades as the machine widens. **This one says the shipped scheduler
exceeds a limit its own design implies** — a different and sharper statement.

The bound is derived from granularities the policy itself picked:
`lag_spread + (ΣT_i + 1) × (QUANTUM + max KernelSection + 2 × RUN_CHUNK)`. It is
crossed at **4, 6, 8 and 12 CPUs**, by 116, 324, 418 and 634 ms (bold in the table
above).

**The gate handles this honestly rather than hiding it**, which is the part worth
preserving. It reds on `max(derived, recorded allowance)`, so a sampled scenario
is gated on not regressing — but `Outcome::fair_over_bound` records every crossing
of the *derived* bound regardless, and the sweep prints
`N ns PAST THE DERIVED BOUND on the recorded allowance`. **The allowance cannot
quietly become the standard**, which is the failure mode of every temporary
baseline and the reason most of them end up permanent.

### `src/build.rs` cannot enable `sched-check`, so no CI run exercises it

The kernel check build is reachable now — `kernel/Cargo.toml:201` forwards
`sched-check = ["toyos-sched/check"]`, and `cpu::MAX_PASS_NS` is 200 µs with
invariant P asserting against it (`cpu.rs:618`, `:1013`). But nothing in `src/`
mentions `sched-check`, so it can only be turned on by hand and the harness never
does.

A check build nobody can run from CI is halfway back to being unreachable, which
is the defect it was built to fix.

### `sys_read` blocks: two doc comments that describe code that is not there

Neither changes behaviour; both mislead a reader about an invariant.

`kernel/src/fd.rs:146` — `/// Insert at the lowest unused id.` It calls
`IdMap::insert`, which is `let id = self.next; self.next += 1` (`id_map.rs:46-51`):
a monotonic counter that never reuses a closed fd number. Lowest-unused is a
POSIX guarantee some code may assume; this is not it, and a long-lived process
leaks fd-number space rather than recycling it.

`kernel/src/process.rs:958` — `/// Must run after `teardown_scheduling`, which is
what flushes the child threads' counters into `ProcessData`.` There is no
`teardown_scheduling` anywhere in the kernel. The ordering requirement it states
may still be real; the function that was supposed to establish it is gone, so the
comment names no enforceable precondition.

### A keyboard flood into a blocked `sys_read` panics the kernel

`prepare_wait` asserts `set_waiting()`, "a task waits on at most one queue"
(`toyos-sched/src/waitq.rs:124`), and a thread blocked in `sys_read` on the
keyboard fd trips it under sustained input:

    !!! PANIC !!!: panicked at toyos-sched/src/waitq.rs:124:9:
    a task waits on at most one queue
      <WaitQueue<…>>::prepare_wait+0x1a5
      <kernel::sched::driver::Ticket>::register+0x9f
      kernel::scheduler::wait_until::<kernel::keyboard::has_data>+0x49
      kernel::arch::syscall::sys_read+0x77
    Running: pid=1 tid=Some(Tid(0))   Syscall: num=1

Seen twice while looking for something else: `Profile::MetalUsb` with QMP
key events injected across the whole boot at a few thousand a second, once
with the i8042 present and once with `q35,i8042=off`, so both the PS/2 and
the USB delivery paths reach it. The victim both times was the in-guest
test runner blocked on stdin at `===READY===`. It does not reproduce at
ordinary typing rates, and neither run was reduced further — the flag is
still set from a previous wait when `sys_read` loops round and prepares the
next one, but which of `wait_until`'s cancel/commit paths left it set was
not established. Reproducing it deliberately means a guest-side key
generator, not a host-side flood.

### An io_uring `Source` can carry one half of the wake pair

Every source needs two wakes at its event site: the direct-blocker queue, and
`complete_pending_for_event` for the ring watchers `process_poll_add`
registered. Nothing in the type system pairs them, so deleting or forgetting
one half leaves a source that looks wired and silently never completes a
`POLL_ADD` — the poller's CQE then arrives only on submit-time readiness (the
immediate post or the TOCTOU recheck) or on close, when `remove_fd` posts
`NotFound`. Otherwise it waits forever.

Both halves are present for every source today. Audio and Network were
restored at the `drain_irqs` site (`kernel/src/sched/driver.rs`); pipes wake
from `process.rs`/`pipe.rs` close paths, keyboard and mouse from
`HidDevice::dispatch_report`, listeners from `wake_poll_waiters`.

History, because it is what makes this worth a line. Stage **7a** (f4d8fa7,
not 7c as this entry and aeeaa01's commit message first said) deleted the
audio and network `complete_pending_for_event` calls out of `drain_events`
while explicitly preserving the keyboard/mouse pair in `hid.rs` — collateral
of the cutover's `EventSource` removal, not an intended deletion; see
`specs/scheduler-migration-log.md`. Neither loss was visible for two months.
soundd polls the audio fd every cycle but its streaming wakes came from its
own armed DLL timer, so only the idle path depended on the missing half —
and there was no idle path until suspend-on-idle. netd polls its NIC fd every
iteration (`userland/netd/src/main.rs`) and waits `u64::MAX` at full idle with
RX being `nic_rx_poll()` only, so a frame arriving while netd is fully idle
posted no CQE and netd slept with `net::has_packet()` true until an unrelated
wake; that never surfaced because no test drives netd, and interactive use
always has something else waking it. The 7c review compared two post-7a trees,
found them identical, and concluded there was nothing there.

The durable fix is iouring-blocking-spec's single `post()`, where a source
cannot have one half of the pair, and a fan-out cannot be deleted without
deleting the wake.

### On an idle machine the log ring flushes one line behind

Measured while building M2's i8042 tests. With no userland process doing
anything, a `log!` line reaches the console only when the *next* piece of work
wakes a CPU — so the most recent line is always still in the ring. Injecting
keystrokes 200 ms apart into an otherwise idle guest and watching serial:

```
0.144  i8042: drain bytes=2 keys=2 woke_kb=1     <- key 'a'
0.347  i8042: drain bytes=6 keys=0 woke_kb=0     <- Pause
0.551  i8042: drain bytes=2 keys=2 woke_kb=1     <- key 'b'
0.754  i8042: drain bytes=2 keys=2 woke_kb=1     <- key 'c'
                                                 <- key 'd' never appeared
```

`drain_chunk_to_serial` runs from the idle loop and the timer path, and an idle
CPU that has just finished its work does not come back for the line it queued
on the way out. The consequence for anyone reading serial: **the last line
before a quiet period is not evidence of anything**, and a guest that wedges
right after logging its final line looks like it never logged it. Every
existing test happens to keep the guest busy, which is why this has not bitten
before; `i8042_no_spurious_wake` drives an in-guest reader for exactly this
reason. Fix is a flush on the transition into idle, not another poll.

### Nothing the kernel logs on the shutdown path ever reaches the console

The same mechanism as the entry above, with a harder ending. `SYS_SHUTDOWN`
(`syscall.rs:219-224`) logs "Syncing filesystems...", syncs, logs
"Shutting down." and calls `acpi::shutdown()`. Both lines go into the ring and
the power goes off before anything drains it. Measured on the MetalDisk profile:
the last console line of a clean shutdown is the kernel's `spawn:` line for
`/bin/shutdown`, and QEMU exits shortly after.

So a shutdown that panics or hangs mid-sync produces no diagnostic at all —
including on the T14, where writing back is the operation with something to
lose. `nvme_large_device` had to assert on the disk image host-side instead,
which is a better assertion anyway but was not a free choice. Fix is the same
flush-before-parking as the idle case, plus an explicit drain before
`acpi::shutdown()`.

### A machine that boots off its internal disk gets no `/boot`

`gpt::probe` runs twice now — `kernel_main` asks the NVMe namespace, and
`fat32_adapter::probe_boot_disks` asks every USB disk — so the stick this
project boots from is found and `/boot` mounts. The NVMe call is the one that
cannot lead anywhere: `page_cache::init` takes sole ownership of the device
immediately afterwards, so even when the boot partition *is* on the internal
disk, `gpt::boot_volume()` names a device nothing can hand the FAT32 adapter.
That is the installed-ToyOS case, which is where this ends up.

The `Resolution::Ambiguous` arm is now live and exercised: `boot_partition_identity`
puts the image's own partition GUID on a crafted NVMe disk while the real stick
carries it too, and the machine correctly reports it has no boot volume. Worth
knowing before adding a third probe — two devices claiming one unique partition
GUID poisons the answer permanently, by design.

### The backup GPT is never consulted

`toyos_gpt::locate` reads the protective MBR, the primary header at LBA 1 and
the primary entry array, and refuses if any of them fails its checks. UEFI puts
a full second copy at the end of the device precisely so that a torn write to
the front is recoverable, and nothing here looks at it: a single bad block at
LBA 1 makes a perfectly good disk unidentifiable.

Not a safety hole — the failure mode is a refusal, which is the safe direction —
but it is the difference between "this stick is worn" and "this stick is
unusable", on the machine class ToyOS boots from. The cost is a second
`parse_header` call against `lba_count - 1` and a second array walk; the design
question is what to do when the two copies disagree, and the answer is almost
certainly to refuse rather than to pick, since a disagreement means one of them
describes a disk this is not.

### Two unreproduced observations

`ps` appeared to stall for >2 s under heavy single-core load; later runs fine. If
seen again, capture with LLDB before restarting.

Doom's music was heard once at roughly half speed. It did not reproduce at HEAD,
with or without `-nodefaults`, and the wav capture measured 1.00x — so whatever
happened, the device-side path was never wrong. Leading hypothesis is host
contention: another agent was building in this tree with a second QEMU running
at the time.

The durable part is the instrument, not the sighting. **Next time, read the
numbers rather than listening**: Doom prints `[music]` synthesis
real-time-factor telemetry every ~5 s, and soundd prints wake/underrun/latency
stats every ~2 s. A starved synthesizer and a wrong playback clock sound
identical to a human, and RTF is what separates them — RTF near 1.0 with the
audio still slow means the clock, RTF well below 1.0 means synthesis is not
keeping up.

---

## 4. Audio and soundd

Spec: `specs/audio-subsystem-spec.md`. Numbered as in the 2026-07-28 audit;
(1) — see the re-filing below; it was never an SQ overrun — (2) `CommandRing::push` assert, (3) ungated
`SYS_SET_RT_PRIORITY`, (4) NaN volume, (7) crash detection and (9) the
"wait until clients have filled" condition are fixed (`97723dc`, `9ed8eda`,
`a88e4ee`, `069d158`).

**CLOSED — audit item (1) was never an SQ overrun; it was silent completion loss
on the CQ, and all three commits it wanted have landed.** Kept as one paragraph
rather than deleted, because the *mislabel* is the durable finding: an entry
filed under the wrong mechanism sends everyone to the wrong ring, and the
submission ring is exactly where you would look. Verified at `ba612c6`
(2026-08-04, by inspection): the mid-registration flush — the cause, not the
protection — is gone from `poll_add_fd`, which now asserts against a declared
`capacity`; the rings are sized from that capacity; and `Poller::wait` reads
`cq_hdr.dropped` and `assert_eq!`s it to zero as the tripwire
(`toyos/src/poller.rs:170`), with a comment saying why it is unreachable. The
stale "asserts rather than overflows" doc comment is gone with it. The
declared-capacity commit was blocked on the compositor/netd bounds and they
landed too — `MAX_PENDING_CONNS`/`MAX_WINDOW_SLOTS` and `MAX_PIPED_SLOTS`.

**BLOCKED ON THE CPAL FORK — one missing message, three consequences.** Killing
wedged clients, suspending on no progress, and resuming from suspend all need the
**same single client→soundd edge**: a resume notification on the control
connection. Recorded as one item deliberately — filed as three, the next planner
schedules three investigations that all reach the same wall.

Established from the code on both sides. soundd's `TOKEN_CMD` carries exactly
three commands — `MixCommand::{AddClient, RemoveClient, SetVolume}`
(`soundd/src/main.rs:151-153`) — and none is resume. The client blocks reading the
soundd→client signal pipe, while cpal's `play()` futex-wakes only its *own*
thread, so **there is no client→soundd traffic in the steady state at all.**

The wake path already exists: `TOKEN_CMD` is what wakes a fully idle mix loop when
a new client connects (`:727`, `:732`). Only the message is missing, and it has to
be sent from cpal's `play()`. **One fork change unblocks all three.**

The three consequences:

1. **Wedged clients cannot be distinguished from paused ones**, so neither can be
   killed. §6.4 *specifies* pause as "no explicit coordination required", so this
   is the spec needing to change, not the implementation.
2. **One paused client defeats idle suspend for the life of the process.**
   `is_streaming()` is `delivered && !pending_removal` (`main.rs:129-131`),
   latched by the first period a client ever supplies and never cleared, so
   `any_streaming` stays true after a pause. Audio spec §5.8's promise ("Zero
   overhead, zero wakes, device voice closed") is defeated: a wake per period
   forever, DMA engine running, codec voice open. It also pins that client's shm
   region, pipe and slot ring, compounding defect (6). **Battery-relevant on the
   T14 specifically** — the machine the metal track is building toward — because
   the wake never stops.
3. **Resume from suspend has no edge to fire on.**

**Correction, recorded because the earlier entry said the opposite.** This file
previously said the suspend half "may be fixable in soundd alone" pending an
answer. The answer is in: **it is not.** A suspended soundd would wait on device
completions that never come while the resumed client blocks forever on a signal
byte soundd is no longer writing — battery traded for permanent silence, which is
strictly worse and is exactly the trade the owner's standing quality rule forbids.

### Stopping the device voice while keeping the timer wake — soundd-only, gate-blocked

Kept out of the cluster above because **its unblock condition is different**, and
that is the useful part: it could land *first* if the quiet tree arrives before
fork access.

Stopping the device voice while keeping the periodic timer wake recovers the DMA
engine and the codec — the battery-relevant hardware — and gives up only the wake
itself. Resume still works unchanged, because soundd keeps writing signal bytes,
so it does not need the missing client→soundd message.

So it is **not blocked on the fork**; it is blocked on the **audio gate**. A
mid-session device stop/restart is an audible transient plus a DLL re-lock, which
needs the thorough tier on a quiet tree.

**A device advertising four buffers panics soundd at startup.**
`assert!(num_buffers > 5, "soundd: pipeline too shallow to defer safely")`
(`main.rs:597`) turns a device shape into a startup panic. Same class as the NVMe
and xHCI zero-device panics closed today — an unanticipated device shape killing a
process rather than being handled — and metal-relevant, since nobody knows what
the T14's codec advertises.

The fix falls out of decoupling the client slot count from the device pipeline
depth, which turns the assert into a clamp. Which is also *why* the assert exists:
`slot_count = num_buffers` (`main.rs:1290`) couples every client's ring geometry to
the kernel's `TX_INFLIGHT_MAX`. The comment's own reasoning establishes
`slot_count >= num_buffers`; **equality was assumed, not derived.** That design is
written up and deliberately not landed — it changes ring geometry and therefore
audio timing, so it needs the thorough gate tier on a quiet tree, not the fast one.

**(5) ASSIGNED — the cpal ToyOS backend hardcodes 44100/2ch/i16** and rejects
everything else, so soundd's resampler and channel-conversion paths (spec §6/§8)
are unreachable from any real client and effectively untested. It also
`assert_eq!`s the device rate against a compile-time constant, so changing the
driver's rate aborts every cpal app.

Deferred to the quiet-tree window, not neglected: editing that fork needs
`.cargo/config.toml` path overrides, which redirect cpal for **every** agent in
the tree. Same scheduling constraint as the fork lint audit
(`specs/fork-lint-audit-plan.md`).

**Client liveness is blocked on this, not on soundd.** The ambiguity between a
paused and a wedged client is *specified*: §6.4 defines pause as "no explicit
coordination required", and the cpal backend's `pause()` is a purely local futex
store soundd is never told about. No change confined to soundd can separate the
two, and landing the soundd and SDK halves alone would kill every paused cpal
client. This is a case where the **spec**, not the implementation, is what needs
to change.

**(6) soundd never frees the per-client shm region.** `SharedMemory::Drop` only
unmaps and nothing calls `destroy()`, so each open/close cycle strands a 2 MiB
page. **Bounded by the next process exit, not by soundd's lifetime** —
`cleanup_process` sweeps every region owned by an exiting process — so this is a
real leak with no release at close time, but not a permanent one. The entry
previously claimed the page was stranded for soundd's whole run, which
overstates it: a long-lived soundd accumulates only until whichever process owns
the region exits.

**ASSIGNED** to the isolation agent, merged with `SYS_GRANT_SHARED`'s missing
revocation: revoke and reclaim are one mechanism, and fixing either alone leaves
the other holding the same page.

**(8) FIXED at `4fce59c`** — `choose_params` (`virtio_sound.rs:62`) now selects a
rate and channel count the device actually advertises, and a device offering
nothing this driver implements logs *which* capability is missing and leaves the
machine to boot without audio, rather than being silently remapped to 44100/2.

> **CAVEAT — not verified by a QEMU boot.** This fix changes device negotiation,
> and seven consecutive boot attempts died in shared-toolchain contention. The
> reasoning that it still selects (44100, 2) on QEMU is *static*, read off an
> earlier boot log's advertised bitmaps. `cargo test -- audio` on a quiet tree is
> owed before this is treated as proven. Recorded as a live gap because an
> unverified change to negotiation is exactly the kind that fails on the one
> machine nobody booted.

**(10) REFUTED — the two TPDF dither draws are independent enough that nothing
can tell.** Kept rather than deleted: the measurement is the finding, and an
entry removed silently gets re-filed next year by the next person who reads
`rng.next() + rng.next()` on one `Xorshift32` state and assumes.

Measured over two million samples, one state stepped twice versus two
independent states:

| | variance (TPDF ideal 0.16667) | χ²/df vs triangular | lag-1 autocorrelation |
|---|---|---|---|
| one state, two draws | 0.16672 | 0.98 | −0.00048 |
| two independent states | 0.16652 | 0.63 | −0.00050 |

The joint distribution of the summand *pair* is where a deterministic
relationship would actually show, and it does not: χ²/df ≈ 1.00 with zero empty
cells at 32×32, 128×128 and 512×512, for both arrangements. The step function
decorrelates the two draws well enough that the pair is empirically
indistinguishable from two independent streams.

**Deliberately not "fixed anyway".** Changing the dither changes the captured wav
bit-for-bit, so it would perturb the audio gate to chase a defect nobody can
demonstrate. This project has been bitten specifically by gates that cannot fail
(`specs/metal-track-history.md`); spending the gate's sensitivity on a
non-defect is the same error wearing a tidier hat.

Two of the three lower-severity items are **FIXED at `4fce59c`**. The passthrough
gain was not a rounding nicety: decoding by 32768 and quantizing by 32767 meant
**32,703 of the 65,536 i16 values did not survive a round trip**, each off by one
LSB. Now 0, gated by an exhaustive host test over every i16
(`soundd/src/main.rs:1347`). `AudioInfo::as_bytes` no longer publishes
uninitialised kernel stack: the padding is spelled out as named fields with a
`const _` size assert, so omitting one is an E0063 compile error rather than a
convention someone can quietly break.

Still open: unknown audio device command bytes report success and do nothing.

**The kernel's byte-1 audio fd verb has no SDK caller.** `kernel/src/fd.rs`
still dispatches `1 => crate::audio::start()`, but suspend-on-idle deleted
`AudioDev::start()` from `toyos/src/device.rs`: the only PCM start left is the
implicit one inside `submit_buffer`, which is what makes resume a single
control verb inline with the first submit. Recorded rather than deleted,
deliberately — a dead-code sweep that removes the arm narrows the ABI, and
the syscall surface is a contract, not an implementation detail. Byte 0
(stop) is live; soundd calls it every suspend.

**Residual from the `069d158` fix:** the deferral predicate cannot distinguish
"mid-refill" from "stopped producing". `9ed8eda` closed most of it by releasing
soundd's read end of the client's signal pipe at the first period the client
delivers, so a dead client is now detectable — but the control thread only
notices when it next reads, and until then the stream stays `is_streaming()` and
the mix loop keeps deferring buffers for a producer that no longer exists.
Bounded harmlessly by `refill_floor_nanos`.

**`f32::round()` lowers to a `compiler_builtins` `roundf` call on the ToyOS
target, not `roundss`.** The quantizer calls it once per sample (256/period,
~344 periods/s ≈ 88k calls/s). SSE4.1 is universally present on the 2020+
hardware baseline, so enabling it in the target spec turns this into one
instruction; whether to widen the target's feature set is a separate decision.

**CLOSED — gate A's fast tier could fail a run on `drains` alone**, with an empty
gap histogram and zero underruns. The proportional-recovery fix (`91a653c`) had
already decoupled drains from harm; the owner's ruling of 2026-08-04 made the
fast tier's verdict harm itself — a mid-tone gap in the capture, or a period
soundd put on the wire with no client audio behind it (`AudioRun::harm`). The
three per-run ceilings are still measured, printed with every run's counters and
kept; what they feed is the thorough tier's `ceiling_runs` rate, unchanged.

Two things moved the other way in the same change, and neither is a loosening.
`underruns` was judged against a ceiling of 12-70 depending on config, so 40
periods of silence on the wire passed a run; it is now judged against zero, which
is what all 120 recorded runs measured. And a run where soundd printed no stats
window at all used to be a ceiling breach — which under a harm verdict would have
passed — so it moved to the instrument-broken set, fatal in both tiers. It was
also the one breach that could enter the thorough tier's sample as a run of
all-zero counters, i.e. as the best run ever measured.

### OPEN — `audio_tone_load (smp=1)` fails gate A's fast tier intermittently, on both trees

Six runs in one session, 2026-08-04, while fix bundle D was being gated. The
fast tier's own two-boot rule is what fails it: dropouts on the first boot *and*
on the confirming re-boot.

```
5408cfb, bundle stashed  RED   6 [1p 3p 5p 6p 11p 41p] /  67 of 1203,  then 1 [5p]      /   5 of 1144
5408cfb, bundle stashed  RED   5 [2p 3p 5p 19p 40p]    / 210 of 1336,  then 1 [5p]      /   5 of 1163
bundle D                 RED   8 [1p×2 2p×2 4p 8p 22p 57p] / 97 of 1231, then 3 [1p 30p 111p] / 142 of 1340
bundle D                 RED   3 [3p×2 10p]            /  16 of 1181,  then 2 [28p 40p] /  68 of 1231
bundle D, in a full suite GREEN gaps: none, wake_lat 5522us (0.24 pipelines), underruns 0/70
bundle D, twice more     GREEN
```

`audio_tone_load (smp=8)`, `audio_tone` at both widths, `audio_idle_suspend` and
`metal_sim_null_audio` were green in every one of the six.

**Two things are established and a third is not.** It is real harm — a gap in
the capture, on two boots running, four times. It is **not fix bundle D**: the
A/B alternated trees in one session on one host, and both sides are red with
overlapping totals. What is *not* established is a rate, or that the four reds
and the three greens differ in anything but when they ran; the host was carrying
three other agents' suites throughout, and the 15-minute load average was still
28 on 14 cores when the greens came in. Under the owner's ruling of 2026-08-04
that is not an excuse and not grounds to re-run it away.

It can fail a landing, since a landing's gate is a full suite. It is almost
certainly the same defect as the entry below, which had one boot and could not
be reproduced; this is four more boots of it, and the leads there apply.
Whoever takes it should get the rate first — the thorough tier
(`--audio-gate N`) is the instrument, and no single run of the fast tier can
say anything about how often this happens.

### OPEN — one boot put 142 ms of silence on the wire, and the host was not quiet

Observed 2026-08-03 while negative-controlling the change above, on tree `602d4e1`
with a harness-only diff — kernel, soundd and the tone client identical to `main`.
`audio_tone_load` smp=1, one boot of four:

```
gaps: total 1 [49p×1]   (142.22 ms of mid-tone silence at 0.813 s)
soundd: wake_lat 46568us (2.01 pipelines)  drains 1  underruns 49  submitted 1203
```

All three instruments agree, which is what makes it one event rather than an
artefact: soundd woke 46.6 ms late — two pipeline depths, so every buffer had
already played out — the pipeline drained once, 49 periods went out with no client
audio behind them, and the capture shows exactly that silence. The recorded sample
for this config is 0/30 dropouts, `underruns` 0 on all 30 runs, and a worst wake of
8250 us; this run is 5.6x that worst wake.

**The host was not quiet.** Another agent's `qemu-system-x86_64` was running in the
primary checkout (observed one second after the run), 1/5/15-minute load averages
6.77/10.19/10.15 on 14 cores. Under the owner's ruling of 2026-08-04 that is not an
excuse and not grounds to re-run it away: the load an audio test puts on this host
is negligible, so a load-coincident stall is a defect of the pipeline until
something shows otherwise. Filed here rather than investigated, per the
one-task-one-agent rule.

Not reproduced in the three other unstaged boots of this config in the same
session (wake 5817, 5280 and 6038 us; `gaps: none`, `underruns` 0 on all three).
The capture was kept by the harness, in its per-pid scratch directory — which is
temporary, so the numbers above are the durable record.

The same session's landing gate carries a smaller instance of the same shape and
no harm at all: `audio_tone` smp=8 at `wake_lat 17050us`, 0.73 pipeline depths and
2.1x the worst wake in that config's recorded 30-run sample, with `gaps: none`,
`underruns` 0 and `drains` 0. Under the verdict above it passes and is printed,
which is the intended reading — one boot, one sample, no audio lost.

The nearest suspect on file is §10's ESP-log flush on the idle path and the
`log_file` flush in `idle_loop` (§3): unbounded, uninterruptible, and in the one
place a `--smp 1` machine spends the time between audio periods. That is a
hypothesis, not a measurement.

### OPEN — nothing explains why doom's audio callback stalled on the T14

doom's sound producer can no longer kill the game when its callback stops
consuming — `doom_sound_flood` stages exactly that and the game lives — and
that is the whole of what is now known. **Why the callback stopped on the
owner's machine, about five seconds into play, is not.** The evidence is one
abort message, because the process that would have carried the answer is the
one that died.

An audio client's RT standing is *lent*, not held: soundd claims the audio
device, takes the RT band with it (`main.rs:705`), and every pipe write it
makes lends the woken reader a one-quantum window (`wake_pipe_readers`, spec
§8.5). **On a machine with no audio device none of that happens.** The null
sink deliberately does not request the band — it protects no audible output —
so `driver::current_is_rt()` is false at its `signal_clients` write and the
client's callback thread is woken as an ordinary thread. The T14 has no audio
device. So the one thing that keeps a 2.9 ms deadline met was absent there and
is present in every config the suite runs.

That is a mechanism, not a measurement, and two others are equally live: §1's
`drain_irqs` entry, where any syscall on that thread can become the USB
driver's engine for as long as a second; and plain scheduling pressure from a
game thread and a compositor that never yield to it.

What would decide it is the callback's own period count against wall clock, on
that machine. doom now keeps that counter (`MIXED_PERIODS`) and now survives
the stall, so the next T14 session can be asked the question instead of losing
the process that would have answered it.

---

## 5. Diagnostics

### OPEN — QEMU 11.0.3 sets `ECAP.PT`, so `Iova::identity`'s comment gives a false reason for correct behaviour

Measured 2026-08-05 off the `hda_probe` boot, in the unit configuration
`specs/iommu-spec.md` §8.1 recorded on 11.0.2:

```
§8.1, QEMU 11.0.2:  cap=0x80d2008c222f06c6 ecap=0x0000000000f00f0a … pt=n
this host, 11.0.3:  cap=0x80d2008c222f0686 ecap=0x0000000000f00f4a … pt=y
```

`ECAP` bit 6 is now set and the kernel's own decode prints `pt=y`. `CAP` moved
too (`…06c6` → `…0686`); which bit that is has not been decoded here, and the
raw words are recorded so the next reader need not take a name for it.

**No behaviour is affected.** §5.7 already writes an identity-mapped domain
"always, and never a passthrough context entry, even on a unit that offers
one", and §8's item 8 carries the argument. What is now false is the *reason*
attached to it: `kernel/src/iommu/mod.rs`'s `Iova::identity` says "§8.1 measured
`ECAP.PT` clear on the only unit anyone here can boot, **so** §5.7's passthrough
context type is unavailable" — a premise this host contradicts, which leaves a
correct decision resting on a reason that has stopped being true. §5.7's own
argument does not depend on it and is the one to keep.

### OPEN — a boot that wedges before the idle loop says nothing at all

Not "says less": **nothing**, including everything it logged before it wedged. The
log ring is drained by exactly two callers — the timer tick
(`arch/idt/timer.rs:138`) and the scheduler/idle loop (`sched/driver.rs:649`) —
and during the boot phases neither runs: `apic::init_timer` calibrates the LAPIC
timer but does not start it (the scheduler arms one-shot timers on demand), and
`enter_idle_loop` is the last line of `kernel_main`. So a boot's output reaches
the wire only when something takes a fatal path, because `apic::halt_all_cpus`
and the panic handler call `serial::panic_flush` and `acpi`'s power path calls
`serial::flush_final`.

A wedge with no panic therefore looks identical to a kernel that never started.
Found at IOMMU stage I2, from a deliberately mis-programmed unit that stopped
NVMe mid-`init`: the guest had logged sixty lines and the harness saw the
bootloader's output and then a ten-second timeout. It costs an hour the first
time and it will cost it again — a wedged boot is exactly the case where the log
matters most.

**Bisecting one meanwhile:** put `$crate::drivers::serial::flush_final();` at the
end of the `log!` macro (`log.rs`), rebuild, and every line arrives as it is
written. `flush_final` is `try_lock` with a bounded spin, so it cannot deadlock
against a holder. A per-phase version — the same call at the end of
`boot_phase!` — narrows it to a phase for a fraction of the output.

The fix is not that patch. A boot-time drain is a decision about where the kernel
may spend microseconds during boot and who owns the backend lock before the
scheduler exists, and the on-screen console already answers the *phase* question
for a machine with a panel (`boot_checkpoint`). Recorded rather than fixed
because the choice belongs with whoever owns the log ring.

### `ps`, `stats` and `dump_blocked` lost their cross-CPU view at Stage 7a

A `CpuSched` is `!Sync` and reachable only from its own CPU, so walking a
sibling's queues is now unwritable rather than racy. `task_cpu_ns` and
`task_sched_state` were rebuilt on values the owning CPU *publishes* —
`TaskHandle`'s counters, republished at each end of a pass, plus the core's
rendezvous word — so they are accurate and lock-free, which also closes the old
`try_lock`-and-skip misreport. `dump_blocked` has no such substitute: it prints
only the calling CPU's parked map, by `TaskKey` and `WaitClass`, with no process
name and no per-source detail, because the pool it used to walk does not exist. A
cross-CPU view costs a message round trip; whether the diagnostic is worth
building is a diagnostics question, not a scheduler one.

### CPU attribution: the recorded "half the CPU is unattributed" claim was wrong

**Its sign was backwards.** Investigated 2026-07-29,
`specs/cpu-attribution.md`. `stop_cpu_timer` adds *one* delta to both the
per-thread `cpu_ns` that `ps` reads and the per-CPU `CPU_TIME_NS` that
`SYS_SYSINFO` reports as busy — they are one accumulator, not two measurements.
Genuinely unattributed kernel time is therefore absent from **both** numerators
and cannot open a gap between them; it pushes the 97% *down*, so true busy
**exceeds** 97%.

The 45-vs-97 gap is reader-side: `ps` divides a since-thread-creation cumulative
by since-boot uptime (`userland/toybox/src/ps.rs:54-56`) while the compositor
taskbar computes a correct one-second delta
(`userland/compositor/src/main.rs:1512-1518`) — a lifetime average against an
instantaneous sample — plus per-row flooring via `as u32` (up to a point per row,
12–20 rows) and reaped processes whose time stays in the system total forever but
vanishes from every row. The recorded prime suspect was also wrong: `mov cr3`
happens *after* `start_cpu_timer`, so the address-space switch is charged to the
incoming task, not lost.

`ps` already fetches `total_cpu_ns` at sysinfo bytes 32..40 and ignores it;
`header total_cpu_ns − Σ(printed cpu_ns)` is exactly the reaped+zombie loss,
measurable today with no kernel change. Real unattributed windows do exist — the
scheduler's pick-and-arm window (deliberate, documented) and the whole idle-loop
body, which does substantial work and is counted as idle — but they are smaller
and different from what the old entry claimed.

### `SYS_PROCESS_STATS` can only report an exited direct child, once

`sys_process_stats` (`kernel/src/arch/syscall.rs:1640`) positions in
`data.child_stats` — a per-parent list, populated only when a child exits
(`kernel/src/process.rs:998`) — and `remove`s the entry it finds. So the
syscall answers exactly one question: what did my own child, which has
already exited, cost? It cannot sample a live process, cannot be differenced
across two calls, and cannot see a daemon at all.

That is the whole of layer 1's read path, and nothing said so outside
`toyos-abi/src/syscall.rs`'s doc comment. `userland/toybox`'s `stats` is a
spawn-and-measure wrapper, which is why it works. Anyone asking "where is
soundd's / the compositor's / netd's time going?" has to reach past it —
`audio_idle_suspend` pays exactly that cost, name-matching `SYS_SYSINFO`
entries into a byte buffer to sample a running daemon twice. A per-process
query on a live target is the missing piece; it is a layer-1 gap, not a
layer-2 one.

### Profiling layers 2 and 3 are not built

Layer 1 (process accounting counters + the `stats` tool) is implemented, with
the read-path restriction above. Event tracing and RIP sampling are not. See
CLAUDE.md's diagnostics roadmap.

---

## 6. Build and toolchain

### An std change that depends on an unlanded ABI change cannot be built at all, from anywhere

`library/std` names its ToyOS dependencies by relative path —
`toyos-abi = { path = "../../../toyos-abi" }` in `rust/library/std/Cargo.toml`
— and `rust/` is the primary checkout's. So std always compiles against
`/Users/jan/Dev/jan/toyos/toyos-abi`, which is main's, no matter which worktree
runs the build and no matter who holds the sysroot. `--claim-sysroot` does not
change it: a claim decides *whose witness is recorded*, not which sources
`x build library` reads. There is no copy step.

The consequence is an ordering nobody is told about: **a change to
`toyos-abi`/`toyos` and a change to std that uses it cannot land in one series.**
The ABI half must reach main first; only then does the primary's tree carry it
and the std half compile. Found on 2026-08-05 by task #140, whose
`clock_epoch() -> Option<u64>` and the `SystemTime::now` that consumes it are
exactly that pair — the std half fails with `no method named unwrap_or found for
type u64`, which reads like a broken checkout and is not one.

The second half of that task's fix, to be applied *after* the ABI is on main.
`rust/library/std/src/sys/time/toyos.rs`, replacing the body of
`SystemTime::now`:

```rust
    pub fn now() -> SystemTime {
        // `UNIX_EPOCH` on a machine that has no wall clock: this signature
        // cannot express the absence, and the kernel reads its clock once at
        // boot, so the answer is the same for the life of the process.
        SystemTime(Duration::from_secs(toyos_abi::syscall::clock_epoch().unwrap_or(0)))
    }
```

Until it is applied `SystemTime::now()` returns `UNIX_EPOCH` on every ToyOS
machine — which is what it did before task #140, so nothing regressed by
leaving it out; it is the improvement that is deferred, not a defect that is
introduced.

**`rust/` is shared, so an uncommitted edit there is everyone's.** That patch
sat in the primary's submodule for about an hour on 2026-08-05 and failed three
other worktrees' landings at `--land`'s step 4, which requires the submodule
clean. `rust/` takes the fork-estate discipline in CLAUDE.md: explicit paths, no
`stash`, and nothing left dirty across a task boundary.

### `--claim-sysroot` livelocks against a second claimant, and the loser cannot build at all

Measured on 2026-08-05 with eight worktrees on the machine. A worktree holding
an edit to `toyos/src` must claim the shared sysroot to build; the claim writes
the witness *inside* the toolchain phase and then the build runs for another
four to six minutes. Anything that claims during that tail leaves the first
claimant refused again — and its next attempt pays a full `rebuild_std` to take
it back.

Observed directly: `--claim-sysroot` returned 0 at 00:39:01 and the very next
command, one second later, refused with "disagree about toyos-abi/src,
toyos/src". Four consecutive claim-then-test attempts lost the witness the same
way. Two earlier attempts in the same session won it, so the outcome is who
finishes last and nothing else.

The cost is not the rebuild, it is that **neither party can run a test between
losing and re-claiming**, so a gate that takes one minute cannot be reached
inside a six-minute cycle that another agent restarts. This is task #134's "no
arbitration" with a measurement on it. The fix wants the same shape as the rest
of the lock directory: the claim and the build that follows it are one hold, or
the witness is checked once and carried through the build rather than re-read
at each phase.

### `Sched::Parallel` tests that go red under other worktrees' suites

Caught by the re-run-alone pass (`specs/test-cost-audit.md` §5.4.6) on
2026-08-04, on a host carrying three to four concurrent full suites, and green
the moment each was re-run by itself in the same process. None predates or was
introduced by the parallel-width work; all have been `Sched::Parallel` since the
phase landed, and none reproduces on a host running one suite.

- **`i8042_mouse`** — REOPENED 2026-08-05, and the closing argument is the thing
  that is wrong. It read: the injection is paced against the guest's own report
  of each packet, "so nothing the host does can outrun the guest". Outrunning is
  not the failure mode left. The host now *stalls* — it waits for a packet the
  guest never delivers and the test times out at 60 s:

      FAIL i8042_mouse: timed out after 60s — 271 of the 303 packets injected
      came back out, so the host stalled on one the machine never delivered

  Pacing converted a lost packet from a miscount into a deadlock, so the verdict
  is now a wall-clock timeout wearing a count's clothes. Measured on a loaded
  host (four worktrees), alternating one checkout against the other in one
  session, six runs: **main FAIL(271/303), PASS(3s), PASS(3s); a branch that
  touches no input code FAIL(976/1004), FAIL(813/845), PASS(3s), PASS(4s)**. It
  reds on `main` as readily as anywhere else, so it is not attributable to
  whatever branch happens to be under it, and a green run costs 3 s against a
  red one's 60 s. The prior text also predates §3's finding below, which says
  the pacing argument never covered `0 lost edges` either.

  Anyone whose suite reds here: re-run it alone before believing it, and if it
  goes green the classification is the bug rather than your change.
- **`usb_transport_break`** — now `Sched::Serial`, and the cause is known: the
  second line is not a second staged break but the driver's *recovery* retrying,
  `transport broke on SCSI 0x2a: command phase completion code 6` against an
  endpoint still halted from the injected one. Recovery still succeeds. So one
  staged break and no other makes "the recovery finished on its first try" part
  of the verdict, and how many tries it takes is how much of the host the guest
  had.
- **`desktop_typing_damage`** — `nothing typed at the terminal window reached a
  shell`. `shell_answers` typed ten times with a flat two seconds between, which
  is a twenty-second ceiling on a desktop coming up; the retry window is now
  `qemu::budget(20 s)`, the phase's. Still `Sched::Parallel`.
- **`desktop_locale_detect`** — added 2026-08-05. Same `nothing typed at the
  terminal window reached a shell`, same `ALONE … GREEN`, in the same run as the
  entry above and on a branch that touches neither the compositor nor the
  terminal. It reaches a shell through `shell_answers` exactly as
  `desktop_typing_damage` does, so it inherits that retry window and evidently
  not enough of it. Still `Sched::Parallel`.
- **`netd_connection_caps`** — added 2026-08-05. Red at 50 s inside a landing
  gate that was otherwise 257/259 with 0 invalidated, green in 7 s alone on the
  same tree moments later, on a branch that touches neither netd nor the
  network stack. The 50 s against a 7 s solo run is the shape of a boot that
  never got enough of the host, not of a cap that was announced wrong. Still
  `Sched::Parallel`.
- **`metal_sim_pointer_churn`** — observed once, on a host carrying three other
  suites *and* a `toyos-sched-sim` run. Not investigated. Still
  `Sched::Parallel`.

**What to do about a red on any of these names:** read the `ALONE` line under it
before anything else. `GREEN` there means the host, not the kernel. What none of
them should get is a widened bound — a gate that tolerates one lost byte
tolerates the defect it was written for. The two fixes above are the two shapes
that are legitimate: make the verdict independent of the rate, or scale a
liveness ceiling with the phase. The global QEMU-slot semaphore this section
used to name as the closing move now exists (`buildlock::guest_slot`,
`specs/test-cost-audit.md` §5.6): the host admits twelve guests across every
worktree, so the four-suite regime these were observed in cannot recur. A looser
assertion is still not the answer.

**But `ALONE … red again — the defect is real` is not evidence, and the protocol
above leans on it.** The re-run happens inside the same process, moments after
twelve guests have been torn down and while another worktree's suite may still
own the host — so it is alone in the suite's bookkeeping and not on the machine.
Measured 2026-08-06 on the xHCI port-machine branch, whose kernel delta is
`drivers/xhci/` and touches no PS/2 and no compositor path:

```
full suite, run 1 (483.7 s for 262 tests):
  FAIL i8042_mouse — 975 of 1004;  ALONE: GREEN
  FAIL screen_early_panic;         ALONE: GREEN
full suite, run 2, the landing gate (512.1 s):
  FAIL i8042_mouse — 560 of 592;   ALONE: red again — the defect is real
  FAIL desktop_locale_detect;      ALONE: red again — the defect is real
then, genuinely alone, same session, minutes later:
  main         a051a67:  i8042_mouse PASS 10.4 s   desktop_locale_detect PASS 11.4 s
  the branch   38431c7:  i8042_mouse PASS  4.1 s   desktop_locale_detect PASS  5.6 s
```

Both trees green on both tests with the host to themselves, and the same suite
that took 120.4 s at the last quiet landing took 484 and 512 s in these two — so
the host was carrying roughly four times its own load throughout, the `ALONE`
re-run included. A verdict that flips between "GREEN, it is the host" and "red
again, the defect is real" for one test on one tree twenty minutes apart is
measuring the host in both directions.

Consequence for the protocol: `ALONE: GREEN` still means what it says, because a
green cannot be produced by load. `ALONE: red again` means nothing on its own
and must be confirmed against `main` in the same session before it is believed —
which is the A/B the audio rules already require and which this line currently
invites an agent to skip.

### A whole parallel phase can be starved by another agent's build

Measured 2026-08-04: the same tree that runs the phase in 44.8 s ran it in
245.2 s with three other `cargo test` processes and a `toyos-sched-sim measure`
on the host. Nothing in the suite reports this — the phase is simply slow, and
whichever load-sensitive `Sched::Parallel` test loses its margin first is what
goes red. `uptime` before and after a suspicious run, and `ps aux -r | head`,
are what separate it from a regression; a `toyos-tests-<pid>` directory per live
suite in `$TMPDIR` names how many are up.

`buildlock::guest_slot` bounds the *guests* to twelve across all worktrees, and
that is the part of this the semaphore closes. It bounds nothing else: a
`toyos-sched-sim measure`, a `cargo build` and the primary's bootstrap all take
cores and no slot, so a phase can still be starved by work that boots nothing.
A slot's wait is announced (`[host-slots] waiting …`), which is what separates a
slow phase from a starved one without `ps`.

### The primary checkout reclaims the shared sysroot silently

Found 2026-08-05 while giving `--claim-sysroot` its arbitration; not fixed.

A linked worktree whose `toyos-abi` differs from the sysroot's must pass
`--claim-sysroot`, which now announces itself and queues behind every run in
flight (`specs/worktrees.md` §3.1). **The primary checkout does the same thing
on any ordinary build and says nothing.** `toolchain::ensure` reaches
`std_sources_stale` for `Owner::Us`, which compares the witness against the
primary's own sources; a worktree's claim makes that comparison stale, so the
next `cargo run -- --build-only` in the primary rebuilds std from main's
sources, rewrites the witness, and the claiming worktree refuses from then on.
No flag, no announcement, and no sysroot lock — `build()` takes it only when
`--claim-sysroot` is passed.

It is the same defect the flag now has arbitration for, on the path used far
more often. It is not fixed here because the two shapes conflict: a suite run
holds the sysroot lock shared for its whole length, and the primary rebuilds std
from inside `build_test_image`, which the harness calls under that hold — so
taking the exclusive lock there would deadlock a suite run in the primary
against itself. The honest fix is that a stale sysroot *inside a run* is a
refusal rather than a rebuild, which changes the primary's daily path and wants
its own gate.

### A daemon's boot lines land in whichever test window is open

`run_test` captures every non-kernel console line between `===TEST_START===`
and `===TEST_END===` as the program's stdout, and the C family compares that
whole capture against an `.expect` file. soundd prints `soundd: ready, ...`
and one `soundd: suspended` once, at its own startup, on the same console —
so whichever test is running then absorbs them and fails on output that is
not its own.

Where they land is a race with no fixed answer. At `dbbdcbe` it was
`71_macro_empty_arg`, mid-C-section of a full run. In the full run at
`5d0c5bd` nothing in the C family caught them. **A filtered single-test run is
the worst case, not a cleaner one**: `cargo test -- 90_stdio_buffering` at
`5d0c5bd` fails with `soundd: suspended` prepended to an otherwise byte-exact
capture, because the one window opened is the one soundd's startup falls in.
Judge the C family from a full run, and read a filtered red for *which* line
differs before believing it.

No cheap honest fix. The kernel tags its own lines `[kernel `, which is why
those are already filtered; userland writes carry no attribution, so a daemon's
line and the child's are the same bytes on the same fd. Either the child gets
a capture channel of its own (the in-guest runner piping and framing its
stdout, which has to keep the line-by-line liveness `run_test_hooked` depends
on) or console writes gain a writer tag. Both are design calls, not repairs.

### CLOSED for build-system bootstraps — a second toolchain-contention window, distinct from the one `69bca9a` closed

`a8c78ef` took the fix this entry asked for and preferred against: the
bootstrap is now serialised across builders rather than made
re-entrant. `toolchain::ensure` decides under the shared build lock and runs
`x.py` under the exclusive one, so two builders' bootstraps cannot overlap and
neither can remove `stage1-std/<target>/dist/deps` while the other's `rustc` is
creating a temp file in it. Every observed instance came through the build
system, so the signature below should be gone.

**What is still reachable**: `./x.py build` typed by hand in `rust/`, which
takes no lock. If the signature reappears, that is the first thing to ask, and
the original preference — bootstrap not recreating a directory it already has —
is still the better fix for it. The record below is kept because recognising it
is the expensive part.

---

`69bca9a` removed the `rustup toolchain link` window — the symlink being
unlinked and recreated on every build, so a concurrent `rustc` proxy landing in
it died with `'rustc' is not installed for the custom toolchain 'toyos'`. That
fix is real and that signature should be gone. **It is not this one**, and the
risk is precisely that the link fix reads as having closed the class.

This window is inside the std bootstrap, and its signature is:

```
error: couldn't create a temp dir: No such file or directory (os error 2)
  at path "<repo>/rust/build/<host>/stage1-std/<target>/dist/deps/rmetaXXXXXX"
error: could not compile `core` (lib) due to 1 previous error
Build completed unsuccessfully in 0:00:43
thread 'main' panicked at src/toolchain.rs:215:5:
std rebuild failed
```

The target varies — seen on both `x86_64-unknown-toyos` and
`x86_64-unknown-none`, which is the tell that it is about the *directory* and
not about any one build. One builder's bootstrap removes and recreates
`stage1-std/<target>/dist/deps` while another's `rustc` is trying to create a
temp file inside it, so the loser dies compiling `core` — the first crate
through, which makes it look like a broken checkout rather than contention.

Recognising it: the path in the error **exists a moment later**. Listing it
after a failure showed `dist/deps` present with a fresh timestamp, because the
winner had finished recreating it. That asymmetry is the same one that
identified the link race (a probe succeeding between failures) and it is the
cheapest check.

Cost so far: two consecutive full-suite runs lost by one agent, plus the seven
consecutive attempts that left `4fce59c` unverified for a session (§4). A third
attempt succeeded unchanged, so it is a race, not a broken tree.

Retrying is what everyone did and it usually worked, but the failure was
expensive because it was diagnosed from scratch each time.

### std leaks a whole thread stack on every `thread::spawn`

`rust/library/std/src/sys/thread/toyos.rs` allocates the stack with
`alloc::alloc` (2 MiB minimum), hands its base to `SYS_THREAD_SPAWN`, and never
records the pointer. `Thread` holds only a tid and has no `Drop`, `join` does not
free it, and the trampoline cannot — it is standing on it. So every spawned
thread costs 2 MiB of heap for the life of the process, which dlmalloc serves
from a dedicated `mmap` above its 256 KiB threshold: one leaked 2 MiB kernel
region per spawn, walking the address space downwards.

Found while testing thread-exit TLS release, where the drift swamped the signal
(the test now drives `SYS_THREAD_SPAWN` directly on a reused stack). It also
makes any per-process memory measurement across a thread-spawning workload wrong.
The fix wants the stack owned by something the joiner can free — a base/layout
pair on `Thread`, freed in `join` after the tid is reaped.

### A fork depended on `toyos-abi` by **git** — the third case the rule does not cover

CLAUDE.md's rule is that forks depend on ToyOS crates *by version, never by path*,
with the reason given: a path escaping the fork's own repo cannot resolve once
cargo checks it out alone. **A git dependency is the third case, and it is worse
than the path case — because it resolves.** Silently, against a frozen snapshot,
with nothing to announce it. `toyos-abi` is the crate where a split-brain does the
most damage.

It happened here. `~/.cargo/git/checkouts/toyos-abi-9a70838a07f829d2/2fe0c57`
holds a `toyos-abi` that is not a slightly older monorepo copy but a substantially
different ABI: **seven files the monorepo does not have** (`gpu.rs`, `message.rs`,
`poll.rs`, `raw_net.rs`, `services.rs`, `shm.rs`, `system.rs`) and **missing two it
does** (`boot.rs`, `io_uring.rs`).

**Not currently live — established, not assumed.** Enumerating all fifteen
checkouts found the git form in exactly two *stale* getrandom commits
(`bb423bc`, `c473bb1`, both `toyos-abi = { git = ... }`). The three getrandom
commits actually pinned (`4659241`, `d304544`, `e05f79d`) all use
`toyos-abi = "0.1"`, and **no lockfile in the tree references
`Japabu/toyos-abi` as a git source at all.** What remains is inert cargo cache.

Filed anyway, because the near-miss is the finding: the violation occurred, ran,
and was corrected without anything in the tree ever reporting either event. The
rule should name the git case explicitly rather than leaving it to be inferred
from the path case's reasoning. Sweep the estate for the pattern when it next gets
path overrides.

### The fork estate is invisible to the zero-warning bar

Cargo passes `--cap-lints allow` to every package whose source is not a *path*
source. All 14 forks in `forks.toml` are consumed as git dependencies, so rustc
discards their warnings before anything can print them. Measured on `sshd`'s
graph: 140 of 143 units capped, the three exceptions being the local path crates
`sshd`, `toyos` and `toyos_abi`.

This is not a build-system defect and no build-system change can reach it. The
build system used to swallow cargo's diagnostics on success as well — that is
fixed — and the forks stayed invisible, because the cap is applied by cargo
upstream of anything `src/build.rs` does.

The trap to avoid is `[lints]` inside a fork: it is a manifest change, so it
lands in `git log <base>..toyos` and would put ToyOS lint policy into every
upstream PR the estate sends. Plan, procedure and the standing-mechanism
question: `specs/fork-lint-audit-plan.md`. It needs a quiet tree, because
path-overriding the forks changes what every build in the repo resolves.

### The `memmap2` fork is 165 lines of unreachable code

`rust/compiler/rustc_data_structures/src/memmap.rs` cfg-gates
`target_os = "toyos"` to a `Vec<u8>` implementation at all 8 sites, and
`rust/Cargo.toml` is the only manifest that patches memmap2 at all — userland's
duplicate entry resolved to nothing and was deleted 2026-08-01. So no ToyOS code
path calls any memmap2 API. `src/toyos.rs` is compiled and never called; the
fork's only load-bearing content is the `0.9.10 → 0.2.1` version relabel that
satisfies rustc's pin.
Either delete `src/toyos.rs` and let `stub.rs` serve, or drop the toyos gate in
`rustc_data_structures` (the only two APIs rustc uses, `map_copy_read_only` and
`map_anon`, are correct in the fork). Exactly one of the two should exist. Three
real bugs in that module were found and fixed 2026-07-28 — see `forks.toml`.

### A deleted guest test binary keeps running until its build artifact is deleted

`discover_rust_tests` enumerates whatever is in
`tests/toyos-rust-tests/target/x86_64-unknown-toyos/debug/`, and cargo does not
remove a binary when its `src/bin/*.rs` is deleted. So a renamed or merged guest
test keeps being compiled into the initrd and keeps appearing in the test list,
from an artifact nothing in the tree can produce any more.

Cost, 2026-08-03: merging three new guest binaries into one left the three
originals on disk, which (a) put ~5 MiB of dead binaries into the initrd and
overflowed the ESP — `Failed to write initrd: No space left on device`, from
`src/image.rs`, which reads as a host-disk problem and is not — and (b) gave a
*machine* test the same name as a stale *rust* test, which silently dropped it
from the run. Both took a while to see because neither error names the artifact.
The ESP's sizing was fixed in the same session and is no longer the tripwire it
was; the stale artifacts are still enumerated.

Fix shape: enumerate from the source directory, or clean the bin directory before
the build. Neither is done.

### CLOSED — two guests in one test process were handed one boot image

`boot_with_options` wrote every boot's disk to one `test-bootable.img` under the
pid's temp directory, and every staged scratch file a test makes — `esp-boot.img`,
`usb-gate-512.img`, the size-keyed NVMe and USB images — sat beside it under the
same fixed names. With one guest at a time that was invisible. The first attempt
at sharing a boot between adjacent tests found it the hard way: a guest still up
when the next one booted had its image rewritten under it, and the new one died
before its first line (`specs/test-cost-audit.md` §5.1).

The QMP socket, the wav capture, the UART log and the screendump were already
per-boot; the image was not, and neither was anything a test staged for itself.

Closed two ways, because the two failures are different. **Per boot**: the
bootable image is now `boot-<seq>.img` and is removed when its `QemuInstance` is
dropped — per boot rather than per thread, since one test may hold two instances
at once. **Per worker**: `tests/common/lane.rs` gives each thread that boots
guests its own subdirectory, and every `test_dir()` in the harness derives from
it, so a scratch name added *after* this still lands in the right place. Reuse
within a lane is deliberate and is what the serial suite already did — the NVMe
scratch is created by the first boot that wants that size and mounted by the ones
after it.

The one place that duplicated the harness's naming, `cache_eviction`'s removal of
the stale T14 namespace, now asks `lane::dir()` for it; it was reaching for
another lane's file otherwise, and its precondition — an *unformatted* namespace
— would have been silently wrong rather than red.

### One timed-out test on the shared boot fails every test after it

`run_test` writes `run <name>` to the guest and reads until `===TEST_END`. On a
timeout it returns and the caller moves to the next name — while the guest is
still producing the *previous* test's output. Every later test on that boot then
reads a window that opens on output it did not ask for, and the whole block goes
red on `exit code Some(0)` and `output mismatch`.

Measured 2026-08-03, at the width the wave-4 work was being calibrated at:
`allocator_stress` (1 s alone) exceeded its 5 s ceiling once, and the run
reported **114 failures out of 238** — one real, 110 of them the cascade, and
three unrelated. The tell is a mismatch whose "actual" is verbatim the previous
test's expected output:

```
FAIL c::01_comment: output mismatch
--- expected ---   Hello ×5
--- actual   ---   4 refusal outcomes decoded, none panicked the client
```

It is not caused by parallelism and predates it — anything that makes one guest
test slow enough to time out produces it. What parallelism did was make it
reachable, which is why the shared block is now `Sched::Serial` (`tests/toyos.rs`)
and why this is written down rather than left to be rediscovered.

Fix shape: after a timeout, resynchronise before the next `run` — read until the
timed-out test's own `===TEST_END`, or make the marker carry the test name so a
window that opens on the wrong one says so. Neither is done. Note the second is
strictly better: it detects the desync instead of hoping the drain caught up.

### A QMP-driven test cannot share a boot with another one

The kernel's log ring sits one line behind on an idle machine (§5), so a guest
that exits the instant it has its answer leaves its last lines — including the
runner's `===TEST_END===` — in the ring until something else runs. On a shared
boot the next member then opens its console window over output the previous one
is still draining into, and reads the wrong thing: measured 2026-08-03 as the
first member passing, the second timing out with its own complete and correct
output visible in the serial, and the third failing instantly on an empty window.

Two workarounds are in the tree, and they are workarounds. `keep_the_ring_moving`
in `tests/toyos.rs` injects keys nothing is listening for, purely so the ring
keeps draining; and the four layout tests take a boot each rather than a group,
which costs three boots. The fix is §5's — a drain that does not need the machine
to be busy.

### A landing's gate is a full suite, and a second suite on the host can time a boot out

`cargo run -- --land` runs `cargo test` inside the integration lock, so a
landing is a 14-minute suite. Nothing serialises it against a suite in *another*
worktree — the lock only serialises landings, and worktrees.md §6 is explicit
that the host is still one host.

Measured 2026-08-03, `--land`'s own landing, with another agent's suite running:
`screen_fatal_halt` failed with `[qemu] Boot timed out waiting for ===READY===`
after 11 s, in a run where 237 of 238 passed in 850 s. The same test alone
passes in 3.3 s, and it had passed in 3 s in the same worktree's previous full
run when the host was quieter. The tell for the contention is in the run itself:
`screen_console_panic` took 39 s against 13 s in the quieter run, the same
binary and the same tree.

So this is the cost §6 predicts, now with an instance. It is left as an
observation rather than a rule because the fix is the counting semaphore §6
already describes and nothing yet hands out slots. Until then a landing that
goes red on a boot timeout is re-run — the isolated re-run is the evidence,
exactly as CLAUDE.md's re-run-in-isolation rule says.

**Second instance, 2026-08-04, and it is not a boot timeout — which widens what
this costs.** `late_storage_connect` failed a landing gate at 20 s with "the
boot scan bound a disk, so the port was not held empty and this gate is
measuring an ordinary boot", in a run where 238 of 240 passed in 693 s; alone it
passes in 5 s. That test stages its disk from the *host* at a moment chosen
relative to the guest's boot, so contention does not merely slow it — it moves
the guest past the window and the staging lands in the wrong place. The test
caught that itself and refused rather than measuring an ordinary boot, which is
the only reason it reads as a red instead of a vacuous green. **A host-staged
timing window is the shape to look for when triaging a landing red, alongside
the boot timeout**, and `Sched::Serial` does not protect it: serial is one guest
per *test process*, and the contention is between processes.

**Third shape, same session, and this one has no mechanism yet.** The very next
landing gate — same branch, same tree, 238 of 240 again — failed a *different*
pair: `usb_flush_optional` with "read the image: No such file or directory" and
`usb_transport_break` with the same `NotFound` out of `tests/common/usb.rs:127`.
Both pass alone (8 s and 4 s). Both are a staged disk image missing from the
lane directory that the same test wrote it to.

What is established: `lane::dir()` is `$TMPDIR/toyos-tests-{pid}[/lane-N]`, keyed
on the *test process* id, and **nothing in the tree removes a `toyos-tests-*`
directory** — grepped, one hit, the constructor. So a second suite cannot be
deleting the first's scratch by name, and the obvious explanation is wrong. The
three shapes so far are a boot timeout, a host-staged window the guest slid past,
and an artifact that is not there; only the first two have a mechanism. Worth an
hour from whoever builds §6's semaphore, because "re-run it" stops being an
adequate answer once the failure can be a missing file rather than a slow one.

Method note, cheap and it cost twenty minutes here: **`pgrep -f "toyos-build
--land"` matches the waiting shell's own command line**, and another agent's
waiter too, so a wait-until-it-exits loop written that way never exits. Match on
`cargo run -- --land`, or count `[q]emu-system`.

### rustup narrates its cargo fallback on every invocation

`info: cargo is unavailable for the active toolchain` followed by `info:
falling back to ".../nightly-.../bin/cargo"`, one pair per cargo call: 5 pairs
in `cargo run -- --build-only`, 249 in a full `cargo test`. The build system
sets `RUSTUP_TOOLCHAIN=toyos` (`src/build.rs:185`,
`tests/common/compile.rs:42`) or passes `+toyos` (`src/libc.rs:29`), and the
linked `toyos` toolchain is rust's stage2 sysroot, which ships `rustc` and
`rustdoc` and no `cargo`.

Recorded rather than fixed, because each way out costs more than the noise:

- **Ask rustup once and reuse the answer.** It will not answer:
  `RUSTUP_TOOLCHAIN=toyos rustup which cargo` fails with `'cargo' is not
  installed for the toolchain 'toyos'`. Only the shim applies the fallback and
  only by narrating it, so "resolve once" means parsing the path out of a
  human-facing `info:` line — a diagnostic used as an interface.
- **Reimplement the fallback rule** in the build system. Duplicates rustup
  policy; when the two disagree the symptom is a cargo/rustc mismatch rather
  than a clear failure.
- **Give the toolchain a cargo** — symlink `rust/build/<host>/stage0/bin/cargo`
  into stage2's `bin/` from `link_toolchain`. Smallest, and arguably the right
  pairing, since stage0's cargo is the one rust's own bootstrap runs against
  this compiler where the ambient fallback is four months older (1.96.0-nightly
  driving a 1.99.0-dev rustc). But it writes into a directory `x.py` owns and
  changes the cargo behind every ToyOS build, so it needs a verification run of
  its own.

Not by redirecting the shim's stderr: rustup reports real errors on it.

### A C test whose compilation fails is skipped, not red

`compile_c_tests` (`tests/toyos.rs`) wraps each compile in `catch_unwind` and
drops the ones that panic, printing a line to stderr and nothing else. Eleven of
the 121 discovered tests are in that state on 2026-08-05 — `78_vla_label`,
`79_vla_continue`, `83_utf8_in_identifiers`, `85_asm_outside_function`,
`89_nocode_wanted`, `94_generic`, `95_bitfields`, `95_bitfields_ms`,
`96_nodata_wanted`, `98_al_ax_extend`, `99_fastcall` — and none of them is in
`C_SKIP`, so nothing in the tree records that they are meant to be failing.

The consequence for anyone changing the compiler: a change that breaks a C test
*at compile time* moves it into this list rather than turning the suite red.
`82_attribs_position` did exactly that during the `__attribute__` work and only
the stderr line caught it. The check that works is to diff the skipped list
across the change; the fix would be to make the list a fixture the suite asserts
against.

`tests/testcases/pp_tcc/` (25 preprocessor cases with `.expect` files) is read by
nothing at all — `compile::testcases_dir()` returns only `tests/testcases/tinycc`
and no other Rust file mentions `pp_tcc`.

### `toyos-cc` is not reproducible: the same source gives a different object

Measured 2026-08-05. One binary, one source file, three runs:
`d_event.c` from doomgeneric produced three objects that differ from each other
at byte 345. The sizes are identical and the code is identical; what moves is
where each BSS symbol lands — `eventhead`, `events` and `eventtail` swap
between offsets 0, 4 and 0x500 from run to run. A `HashMap` iteration order
somewhere in `codegen` is the obvious suspect and was not chased.

Across doomgeneric's 82 sources, comparing one binary against *itself* on two
runs, 14 objects differ by disassembly. So a byte-for-byte object diff is not
available as evidence that a compiler change was inert, and anyone who tries
one will find 40-odd differences and no change behind them. What does work is
diffing preprocessed output, which is stable — that comparison was byte
identical on all 82 across the `__attribute__` change.

Consequences beyond that: no build cache anywhere can trust an object hash, and
a miscompilation that depends on symbol placement would not reproduce.

### `toyos-cc` does not implement packed bitfield layout, and says so

`__attribute__((packed))` on a struct with a bitfield member is refused —
`resolve_struct` in `toyos-cc/src/codegen/resolve.rs`, covered by
`toyos-cc/tests/attributes.rs`. Packed and unpacked bitfields are different
algorithms rather than one algorithm with a flag: gcc allocates a packed
bitfield's bits contiguously from the current bit position and lets a field
straddle what would have been a storage-unit boundary, where
`walk_struct_layout` picks a storage unit of the member's own type and starts a
new one whenever the next field would not fit. `codegen/bitfield.rs` loads and
stores through `clif_type(storage_ty)` at the field's byte offset, which a
straddling field has no single unit for.

`specs/wlan-plan.md` §10 counts 635 `__packed` uses in the AX210 subset. However
many of those carry bitfields is how much of this W6 needs.

### `toyos-cc` does not define `__GNUC__`, so doomgeneric compiles unpacked

`PACKEDATTR` in `userland/doom/include/doomtype.h` and in doomgeneric's own
`doomtype.h` is `__attribute__((packed))` under `#ifdef __GNUC__` and empty
otherwise, and toyos-cc seeds neither `__GNUC__` nor `__GNUC_MINOR__`. Measured:
preprocessing `w_wad.c` through toyos-cc yields zero occurrences of either
`PACKEDATTR` or `__attribute__`, and `} PACKEDATTR wadinfo_t;` arrives at the
parser as `} wadinfo_t;`.

This is inert today and was checked rather than assumed. Compiling doomgeneric's
fourteen `PACKEDATTR` structs with clang twice, once with the macro empty and
once with it expanded, moves **no field offset at all** and changes one size:
`pcx_t` is 130 unpacked and 129 packed. `WritePCXfile` never takes
`sizeof(pcx_t)` — it writes the header field by field and derives the length
from its own pack pointer, and `offsetof(pcx_t, data)` is 128 either way. The
remaining thirteen differ only in alignment, and every one of them is read
through a pointer into a WAD buffer.

Defining `__GNUC__` would be a much larger change than it looks: it turns on
every `#ifdef __GNUC__` block in doomgeneric and in any header that has one.

---

## 7. Design debt

### io_uring abuses shared_memory

io_uring does not share memory between processes — it shares a page between the
kernel and one userspace process. It should own its `PageAlloc` directly, map it
into the process's page tables, and store it in `IoUringInstance`; Drop frees the
pages. This also removes the only caller of `shared_memory::destroy()`.

### `SharedToken` is a bare `u32` with no RAII

Unlike `PhysPage`, which cannot leak because Drop returns it to the PMM,
`SharedToken` is `Copy` with no destructor, so the caller must remember to call
the right cleanup function. It should be a non-Copy RAII handle whose Drop
removes the region and frees the backing pages, exposing `.raw()` for the numeric
value to hand to userspace while the owning handle stays in kernel structures.

### `Fd` is a Unix-ism

ToyOS has no files-are-everything model. The integer identifies pipes, devices,
io_uring instances and IPC connections — it is a handle, not a file descriptor.
Rename `Fd` → `Handle`. Aligns with the capability-based direction.

### `gpu::set_resolution` frees the old framebuffer while consumers may hold pointers to it

`kernel/src/gpu.rs:59-76` calls into the driver, and virtio's implementation
allocates a new framebuffer and frees the old one. Today the only consumer
re-reads `GpuInfo` afterwards, so nothing breaks; the pattern is simply
unguarded for anything that caches the address. The panic console is the first
thing that would have cached one, and it handles the window explicitly —
`detach()` before the call, `rearm()` if the driver refused — which is a
per-caller workaround, not a fix. The fix is for `set_resolution` to own the
invalidation.

### `KernelSlice::from_raw` cannot check the one thing that makes the type safe

`kernel/src/mm/region.rs:16` (the live `TODO`s at `:12` and `:15`). Every bounds
check `KernelSlice` performs is against a size the caller asserted; `from_raw`
cannot validate it against the allocation, so a slice longer than its buffer
passes every check the slice makes. Three call sites, each correct only by
adjacency: `OwnedAlloc::slice` (`process.rs:70`, the one site with an assert),
the ELF loader (`elf.rs:1005`, size and allocation share `load_size` by
proximity, not by construction — and every past OOB in the loader came through
this type), and `DmaPool::alloc` (`drivers/mod.rs:34`).

Fix shape: allocators construct the slice. Give `PageAlloc` and the contiguous
PMM path a `slice()` method like `OwnedAlloc`'s, sized from the allocation they
own, then make `from_raw` private to `mm` or delete it. The loader and DmaPool
stop naming sizes at all.

### Nothing can ask which layout a *surface* is translating with

Half of this closed with the input rework and the half that is left changed
shape. `SYS_SET_KEYBOARD_LAYOUT` is deleted; the layout is
`toyos::surface::LAYOUT_CONFIG`, a file, and anything may read it — so `locale`
could print the configured name today, and the interactive menu could open on
it. That is a small piece of work nobody has done.

What no file can answer is what each *translator* is actually using. There is
one per surface and each re-reads the config when its host says so, so a
terminal that missed the notification disagrees with the file and nothing can
see it. `specs/introspection-plan.md` §1's `SYS_QUERY` is still the shape that
answers "what is this process holding", and it is still not built.

### CLOSED — `locale detect` cannot run under the compositor or `/bin/console`

Closed by `toyos::surface`, the channel a surface owner serves to its children:
a child asks for raw transitions, the host grants or refuses, and while the
grant is held the surface stops translating. The entry's own diagnosis was that
the terminal is what loses the usages, and that closing it meant "a way for a
terminal client to ask for raw usages" — which is what this is.

`locale_detect` (the wizard under a host that holds the keyboard),
`console_locale_detect` (under `/bin/console`) and `desktop_locale_detect`
(under `/bin/terminal`, three processes below the compositor) are the gates.
The last two also assert that the layout the wizard chose is in force
afterwards: the key a US board prints `[` on types `ü`.

### The console font cannot draw most of the Swiss German AltGr layer

`src/assets.rs`'s `console_font` rasterises U+0000..=U+00FF plus box-drawing and
block elements, and `font::draw_char` substitutes `?` for anything else. The
`swiss-german` table is faithful to xkeyboard-config's `ch(de)`, which reaches
well past Latin-1: `€`, `⅛`, `œ`/`Œ`, `ŋ`, `ħ`, `ł`, `ŧ`, `đ`, `ĸ`, `ſ`, `ẞ`,
`Ω`, the arrows on `i`/`u`, and the typographic quotes on `b`/`n`/`v` all render
as `?` on the panel. So do most dead-key compositions outside Latin-1 — `ĉ`, `ń`,
`ẑ`, `Ÿ` and the superscripts — while `â ä à é ç ·` and the rest of Latin-1 are
fine. The bytes delivered to the application are correct in every case; only the
glyph is missing. Widening the rasterised set is the fix; it is a build-time
list, not a code change. `legends_are_renderable` in
`toyos-keymap/tests/detect.rs` keeps the wizard's own prompts inside the covered
range, and it is the only thing that does.

### CLOSED — `locale <name>` persists, `locale detect` does not

Both write `toyos::surface::LAYOUT_CONFIG` now, through one `set()`, and neither
tells a translator *which* layout to use — each is told the file moved and
re-reads it, so two surfaces cannot hold different answers to a question the
user asked once. `locale --load` is gone with the init line that ran it: there
is nothing to load into, because a translator reads the config when it starts.

The `/home` caveat is unchanged and still true: it is tmpfs on the T14, so the
choice survives a login and not a reboot.

---

## 8. Hardware and performance gaps

### `i8042_mouse`'s pacing argument covers its count and not its `0 lost edges`

Red in the wide phase, green alone, on a tree whose only kernel change was the
scanout's memory type:

```
FAIL i8042_mouse: 1008 packets, none of them sent before the one before it
arrived, and the driver does not report `0 lost edges`:
  i8042: 1942 interrupts, 3056 bytes, 24 keys, 1006 motion, 12 undecoded,
         0 discarded, 0 overruns, 0 dropped, 1 lost edges
```

Alone, the same guest: 2014 interrupts, the same 1006 motion events, `0 lost
edges`. `main` at 91796eb ran it green in the wide phase with 1995 interrupts —
so it is marginal rather than broken, and it is not the scanout change, which
touches no port I/O.

The `Sched::Parallel` beside it argues that "none of the three measures a rate"
because each packet is sent only after the guest prints the one before it. That
is correct about the *count*, and all 1006 arrived in both runs. It does not
reach the other assertion: `lost edges` is not a delivery figure but the
driver's own count of a counter line repeating without an interrupt having
arrived — an edge the pin asserted and nothing delivered. Twelve guests on
fourteen cores can produce one of those without any packet being lost, which is
exactly the shape seen.

Two ways to close it, and the choice belongs with the suite work rather than
here: move `i8042_mouse` back to `Sched::Serial`, or split the verdict so the
paced count stays parallel and the lost-edge claim runs where the host is not
oversubscribed.

**2026-08-05: the *count* now fails too, which the second option above would not
have caught.** On the `hda-probe` branch, with eight worktrees live on the host:

```
FAIL i8042_mouse: 996 pointer events reached userland out of 1004 packets
injected, each one paced against the arrival of the last
  FAIL  i8042_mouse  (60s)
  …
  [i8042] 1008 packets injected, 1008 out, last button state 0x00
  PASS  i8042_mouse  (570ms)
  ALONE i8042_mouse: GREEN
```

Sixty seconds in the wide phase against 570 ms alone, and eight of 1004 packets
missing rather than an edge miscounted. So the pacing argument does not hold for
the count either under this much contention — the guest is slow enough that
something upstream of the count gives up before the packet arrives. Splitting
the verdict therefore fixes only half of it, and `Sched::Serial` is the option
that closes both. The branch that saw it changes no port I/O and no input path:
its only ungated change anywhere is one extra field on the `iommu: unitN` line.

**And on the same afternoon, on the USB mass-storage branch, the isolated run
was red as well** — which takes contention away as the remaining explanation.
Same host, a branch whose kernel delta is `drivers/xhci/` plus a gate-only
feature:

```
wide phase (253 tests, 235.5 s, other agents building):
  FAIL i8042_mouse: 1002 pointer events reached userland out of 1004 packets
  injected, each one paced against the arrival of the last
re-run alone:
  FAIL i8042_mouse: timed out after 60s — 986 of the 1004 packets injected
  came back out, so the host stalled on one the machine never delivered
its own retry, in that same process:
  PASS i8042_mouse (552ms)
```

Eighteen packets short with the host to itself, then green 552 ms later. So
`Sched::Serial` would not have closed it either. What the two sightings share is
the shape — the budget spent waiting for one arrival that never comes — so the
thing to find is what drops a packet on a guest nobody is contending with,
rather than which phase the test runs in.

### Device registers still take firmware's word for being uncacheable

Every `map_mmio` outside the scanout passes `CachePolicy::DeferToMtrr`, which is
PAT entry 0 — WB, the entry that takes whatever the MTRR says for the range.
That is correct only while firmware covers the PCI hole with a UC range
register. Nothing checks that it does. A BAR in a range no MTRR covers, under a
`MTRRdefType` of WB, is a set of device registers the CPU is free to cache and
to reorder, and no symptom names the cause.

It survived review for years because there was no alternative: with the reset
PAT there is no way to *say* UC without PCD or PWT, and the kernel wrote no PAT
at all. That is no longer true — `arch::pat` owns the table, and a third
`CachePolicy` selecting a UC entry would make every BAR uncacheable whatever
firmware did, at no cost, since Table 11-7 gives UC for a UC PAT entry under
every MTRR type. It is not in this change because nothing has been observed to
be wrong: `mtrr::range_type` can answer per BAR and no boot has been asked.

The measurement that decides it: log `range_type` for each BAR `map_mmio` is
given, on the T14 and on QEMU, and see whether any comes back other than UC.
`specs/userspace-drivers-spec.md` §"It works because firmware's MTRRs make the
PCI hole uncacheable" and `specs/iommu-spec.md` both record the same
dependency; this is the entry that says the machinery to remove it now exists.

### A BAR sharing the scanout's last 2 MiB page is a boot panic

`map_2m` refuses to put two different non-default memory types in one 2 MiB
page: whichever call ran last would decide, and the other would write through a
type it did not ask for — which for a device BAR means combined, reordered
register writes. The refusal is a panic naming both entries.

That is the right failure and it is reachable from firmware's BAR layout rather
than from any kernel bug. The scanout is mapped write-combining by
`panic_console::remap`, which runs before every driver's `map_mmio`, so a BAR
placed inside `[fb, fb + align_2m(fb_size))` panics the boot. The layout that
would do it is a small BAR immediately after the framebuffer's, close enough to
land in the same 2 MiB page — which BAR alignment rules make unlikely, since a
framebuffer BAR is large and the next one starts on its own size boundary, but
does not forbid.

Not staged on either machine: QEMU's stdvga puts nothing there, and the T14 has
not been asked. If a T14 boot ever panics in `map_2m` naming `0x4000000000`,
this is what happened, and the fix is to give that page `DeferToMtrr` and take
the framebuffer's last page back to UC rather than to widen the check.

### CLOSED — `build_toyos_bins` read a `.so` another build was replacing

`src/build.rs`'s cdylib sweep did `fs::read_dir(&lib_out).unwrap()` and then
`fs::read(so_entry.path()).unwrap()` on each entry, and between those two a
concurrent build in the same tree could replace the file:

```
thread 'main' panicked at src/build.rs:786:54:
called `Result::unwrap()` on an `Err` value:
  Os { code: 2, kind: NotFound, message: "No such file or directory" }
```

Seen once in four deliberately concurrent `cargo test -- xhci_slow_connect`
processes — one died, three passed.

Closed by the treatment the staged kernel and bootloader already had:
`buildlock::artifact` is now held across each build→read pair in
`build_toyos_bins`, both the cdylib sweep and the test binaries, so no other
builder in this worktree can land between the `read_dir` and the `read`. The two
`unwrap`s that produced the message above name their file now — a bare
`Result::unwrap()` on a `NotFound` naming nothing is what made this hard to read
in the first place.

**What it does not cover**, and the reason it is worth saying: the lock is
advisory and per worktree, so a `cargo build` typed by hand in
`tests/toyos-rust-tests/` still races it. That is the same exemption every other
`buildlock` user has.

It was the fourth member of the "a red build may be the build system, not the
code" family, and the tell was the same: the same command succeeded on the next
attempt.

### Two framebuffer clients still pay the scanout's price, and the panic console pays it worst

Closed for `/bin/console` in `45b5010`: the terminal emulator composed against
the panel, so `Framebuffer::scroll_up`'s `ptr::copy` read back every byte it
moved — 16,777,212 bytes read and 16,820,337 written per scrolled text row,
counted in QEMU at 2048x2048. It composes in system RAM now and blits damage
through `window::Screen`, which has no read path at all. What follows is what
that left behind.

**The compositor holds the raw scanout as a `Framebuffer` and reads it.**
`userland/compositor/src/main.rs:740` builds one over the GOP mapping, and two
paths read through it. `draw_software_cursor` (`:719`) calls `get_pixel` per
cursor pixel to blend against what is underneath. And `Framebuffer::fill_rect`
copies its first row to every subsequent row, so a full-screen `clear` reads
back one row short of the whole surface — the 16.7 MiB read the console's
startup used to spend, measured before the fix. Neither shows up under QEMU,
where the framebuffer is host RAM. The fix shape is the console's: compose in
system RAM, hand `Screen` finished pixels. It is a larger job there because the
compositor's damage model is per-window rather than per-cell.

The compositor now says what those paths move, from inside. Every ~2 s, from a
composited frame, `FrameStats::report` prints `scanout_rd_bytes` — split into
`rd_px`, the cursor's `get_pixel` calls, and `rd_bulk`, `fill_rect`'s row
copies — beside `scanout_wr_bytes`, the frame count and min/max/total composite
time. Closing this entry is what takes all three read figures to zero.
`metal_sim_compositor` requires the line and prints it: three frames at
1920x1080 read 747,144 bytes back for 9,531,840 written, reproducing
byte-for-byte between runs while the times do not. They are byte counts and
never a cost — the cost is the uncached read, which QEMU cannot have. Both
figures are lower bounds, and anyone deriving from them needs to know by how
much: they count what goes through `Framebuffer`, so glyphs (`put_pixel`,
uncounted by design) and the title-bar icons (handed `screen.ptr()`, and
alpha-blending through it, so they read the panel as well as write it) are in
neither. The 1920x1080 figures above have no window on screen and therefore no
icons; a desktop's do.

**The panic console's repaint is ~460 ms on the T14**, measured from inter-line
gaps in both boot logs (461 ms and 459 ms) in
`specs/metal-hardware-inventory.md`; five of the six `boot_checkpoint`
repaints fall inside the reported 3422 ms boot, which is most of it.

This is *not* the same defect. `kernel/src/drivers/panic_console/mod.rs` never
reads the framebuffer — it writes `core::ptr::write_volatile` one `u32` at a
time, in `fill_screen` (`:769`) and per glyph bit (`:791`). 1920x1080 is
2,073,600 of those per full repaint, which at 460 ms is 222 ns each: the cost
of a store to an uncached mapping.

**The mapping is write-combining now, which changes what is left of this.** The
kernel programs `IA32_PAT` entry 4 to WC on every CPU and maps the scanout
through it — its own direct map and every process holding a token — so a store
to the panel merges with its neighbours instead of being its own bus
transaction. `fill_screen` writes a row of `u32`s in address order and is what
gains most; `draw_glyph` writes one `u32` per set bit across sixteen rows and
gains least, because scattered stores are what WC cannot merge. Neither figure
exists: QEMU's framebuffer is host RAM and can show neither, so what is open
here is the measurement off the T14 rather than the mechanism.

That makes the painter's granularity the live question again — a glyph
assembled in a scratch row and blitted as one run would merge where the per-bit
writes do not. Note the constraint that rules out the obvious scratch buffer on
the panic path: it takes no lock of any kind and paints from contexts where
nothing may be waited on, so a shared static strip would add exactly the
multi-CPU race §2 already records against `capture()`.

Belongs to #65 (boot time) rather than to the console work that found it.

### The collapsed-scroll paint is reached only by the boot seed, and nothing asserts the panel after it

`Console::write_bytes` flushes once per call, so a batch carrying more rows than
the screen has scrolls the cell grid many times and reaches the panel once. That
path is the reason `Console` composes in RAM at all, and `screen_console_scroll`
was written believing its last round — a single 1200-line write — exercised it.
It does not. The console reads its shell's pipe with `let mut buf = [0u8; 4096]`
and one `read` per poll pass, so the batch it sees is capped there whatever the
writer does: instrumented on 2026-08-03 at 1781 bytes a batch when the writer
flushed every 7 lines and **2870** when it wrote all 1200 at once — 112 reads for
one write. At ~257 bytes a line that is 11 lines, against the 66 rows a collapse
needs.

The only caller that does reach it is `seed_kernel_log`, which hands the console
up to 64 KiB in one `write_bytes` at startup — a thousand rows scrolled into a
single paint. No test asserts the panel after the seed: `screen_console_shell`
and `screen_console_scroll` both wait for the prompt and then assert on what
comes *after* it. So the path exists, runs on every console boot, and is checked
by nothing.

Reaching it from a test needs a writer inside the console's own process, or a
console read larger than a batch. Not a defect in the console; a hole in what
the screen family covers, and the workload comment that claimed otherwise is
corrected as of this entry.

### `Console::flush` records the rightmost column as painted when the scrollbar clamped it away

The blit is clamped to the bar's left edge and the bookkeeping is not:

```rust
for col in first..=last {
    ...
    self.painted[row * self.cols + col] = Some(cell);   // every col
}
let w = (span * fw).min(paint_width.saturating_sub(x));  // clamped
if w > 0 { self.screen.blit(x, row * fh, w, fh, span * fw, &strip); }
```

With the bar up, `paint_width` is `1920 - 6 = 1914`, so glyph column 239 starts
at x=1912 and receives 2 of its 8 pixel columns — while `painted[239]` claims
the whole cell was delivered. That is exactly the class `screen_console_scroll`
exists to catch, written into the emulator.

It self-heals, which is why no test sees it: `flush` fills `painted[.., 239]`
with `None` on every transition of `bar != self.painted_scrollbar`, and the bar
appears and disappears around any paging. The window is between two such
transitions — successive `scroll_view_up` calls with the bar already up, where
column 239's content changes and the panel keeps the old glyph. One column, only
while paging, gone as soon as the view returns to the bottom.

The fix is to clamp the loop rather than the blit, so a cell that was not
delivered is not recorded as delivered. Left open because it is a one-column
window under an in-progress gesture, and because the honest test for it has to
assert the panel *while* the view is off the bottom, which nothing does yet.

### The UEFI GOP path is off by default, and picks an absurd mode when on

Until `06ce633` no configuration in this tree produced a UEFI GOP at all:
`kernel/src/drivers/gop.rs` had never executed and `kernel_args.gop_framebuffer`
was zero everywhere. `cargo run --gop` and `BootOptions { profile: Profile::Gop }`
(`-vga std`) fixed that and the path works, but two residuals remain.

**It is not the default.** Plain `cargo run` and the default test config still
boot `-vga none` with virtio-gpu or with no display device, so `gop.rs` is
exercised only by `--gop`, by `--metal-sim` (which every machine test now
boots), and by the screen tests that boot a guest at all. Every other test in
the suite still says nothing about the display path a laptop takes.

**The mode is wrong.** `bootloader/src/main.rs:186-205` selects the mode with
the most pixels. On QEMU stdvga that is **2048x2048** — square, non-standard,
and it makes the compositor scale a 1920x1080 wallpaper to a square. It is also
16 MiB of framebuffer, which is what makes a panic-console repaint cost ~13 ms.
"Largest wins" is not a mode policy; a real one would prefer the firmware's
current mode, or the largest 16:9/16:10 mode, and only then fall back. Harmless
for M0, wrong for M1 — and M1 shipped without fixing it, so the compositor
scales a 1920x1080 wallpaper onto a 2048x2048 square on every metal-sim boot
and each panic screendump is 12 MiB. On the T14 the firmware will offer the
panel's own mode and "largest wins" may or may not pick it; that is the part
nothing here can answer.

**What the repaints actually cost.** `e5e600f`'s message gives two figures that
cannot both be true — "~13ms per repaint" and "135ms to 181ms" for six of them.
Measured A/B in one session on this host: the same tree boots to `Boot:
complete` in **118 ms** with no console armed (`-vga none`) and **188 ms** under
GOP. Five repaints happen before that line is logged and the sixth after it, so
the per-repaint figure is right (~14 ms) and the 135→181 pair is the wrong one.
Every phase boundary also carries a `wbinvd`, which QEMU ignores entirely — on
metal that is six full cache-hierarchy flushes on a machine that keeps running,
and it is not measurable here.

### A device claim succeeds on a machine that has no such device

`device::try_claim` gates `DEVICE_FRAMEBUFFER`, `DEVICE_NIC` and `DEVICE_AUDIO`
on an info struct the driver registered, so those three return
`ClaimError::Absent` when the hardware is absent — which is what makes soundd
and netd able to exit cleanly.
`DEVICE_KEYBOARD` and `DEVICE_MOUSE` are gated on nothing at all: they hand out
a `Descriptor` whether or not any driver will ever produce an event. Under
metal-sim the compositor holds both claims on a machine with no HID of any kind
and polls them forever. Harmless today, wrong in the same way the isolation
issues in §1 are wrong: a claim is supposed to be evidence.

### Every network client pays a second of boot retry on a machine with no NIC

`NetdConn::connect_blocking` (`toyos/src/net.rs:271`) retries `services::connect`
100 times at 10 ms. That is right when netd is merely slow to start and wrong
when it will never start: under metal-sim sshd sleeps 100 times, exits at
t=1.69 s on a boot that reached `Boot: complete` at 0.38 s, and its 100
`SYS_NANOSLEEP` calls are the whole of its accounting. Cheap fix: have netd
publish "no NIC" rather than not publishing at all, so the retry has something
to observe.

### PARTLY CLOSED — the xHCI driver never gives a slot back

**A slot is now given back when its device is unplugged, and only then.**
`0ed2bc1` added Disable Slot and made `device::configure` carry the slot id out
of *every* path below the successful Enable Slot, including the eleven refusals
below — so the port remembers the slot whether or not a device came of it, and
`teardown_port` disables it. `xhci_hotplug` shows the controller handing the
same slot id straight back to the next device plugged into that controller.

What that closes is the hotplug half, which was the half that grows: without it
every plug cycle cost a slot and 64 of them exhausted a PCH controller. What it
does not close is a device that is **still plugged in** after a refused
enumeration — a hub, a camera, a fingerprint reader, or any of the eleven paths
— which keeps its slot until it is pulled. That is the entry below, unchanged,
and the count is still 11.

`init_device` enables a slot for every connected port and issues no Disable
Slot, on any path: not for the devices it walks past (a hub, camera or
fingerprint reader), not when Address Device fails, not when the descriptor
fetch fails, and not when the slot id comes back past the pool's device blocks
(the `layout.device()` `None` branch). Each of those keeps a slot for a device
the driver will never talk to again. Mass storage added three more: a disk
whose interface has no bulk pair, one the pool has no mass-storage block for,
and one that fails `bring_up` — and the boot stick came *off* the list, since
it now binds.

The fourth is the one with a test behind it: `xhci_slot_exhaustion` leaves five
slots enabled with a zero DCBAA entry every run, which makes the entry's own
test the largest producer of the leak it describes.

**The count is 11, not four plus three**, enumerated in
`specs/type-safety-audit/usb-storage.md` F12 by reading every path between the
successful Enable Slot and a bound device. Three of them are named nowhere
else: SET_CONFIGURATION failing (`device.rs`), Configure Endpoint failing for
the bulk pair (`msc.rs`) and for the HID interrupt endpoint (`device.rs`), plus
`PointerSource::claim` running out. A fix that adds Disable Slot to the four
named above leaves seven behind, and this entry is what somebody will work
from.

Harmless where slots outnumber ports, which is every machine in reach: QEMU
reports 64, Intel's PCH controllers 32 or more, and no root hub has that many
ports. It stops being harmless on a controller whose slot count is below its
device count, where a HID on a later port loses its slot to a hub on an earlier
one. `xhci_slot_exhaustion` is what would catch the regression — it proves the
machine survives the shortage and that the one device which fit was enumerated
to completion, not that the right devices win it.

### The xHCI driver refuses a controller whose PAGESIZE does not include 4 KiB

`init` logs OP_PAGESIZE and refuses the controller by PCI address if bit 0 —
the bit that says 4 KiB — is clear. Every structure the driver places — rings,
contexts, scratchpad buffers — is sized and aligned to a hardcoded 4 KiB, so a
controller that cannot do 4 KiB is unimplemented, not merely unusual, and the
machine says so at init instead of corrupting memory silently.

**It used to `assert_eq!(pagesize, 1)`, which was wrong twice** and is fixed at
`5fde1c5`. The register is a *mask* of the page sizes the controller supports,
so equality is stricter than the requirement (Linux reads it with `ffs()`);
and a panic takes the machine for one controller's property on a laptop that
has two, which is the exact failure the drive-every-controller work exists to
prevent. Sixty lines above, a controller with neither MSI-X nor MSI is refused
by name and `init` carries on. Both are equally fatal to that controller and
neither is fatal to the machine; now both read the same.

The scratchpad is the whole exposure. Its entries are one 4 KiB page apart,
so at PAGESIZE 8 KiB with `max_scratchpad = 8` entry 7 sits at 0xF000 and the
controller writes [0xF000, 0x11000) — over entry 6 and into block 0's
interrupt ring at `dev_base`. Every other consequence runs the safe way: a
larger page size only relaxes the rule that the DCBAA and the device contexts
must not cross one.

What is still not built is honouring such a controller. If a machine ever
trips the assert, the fix is to derive `PAGE` from the register instead of
raising the bound.

### PARTLY CLOSED — the xHCI driver reset the controller without taking it from firmware

Closed at `755b591` + `d83c53b`: `kernel/src/drivers/xhci/legacy.rs` walks the
extended-capability list from `HCCPARAMS1.xECP`, and when it finds capability
ID 1 it sets the OS-Owned Semaphore, waits a bounded second for the BIOS-Owned
Semaphore to clear, and clears USBLEGCTLSTS's SMI enables and its RW1C status
bits whatever the semaphore said. It runs immediately before the halt and
HCRST, an absent capability and a malformed list both cost the handoff and
never the boot, and the driver proceeds either way — a machine that will not
boot is worse than one whose firmware is fighting it, and the point is the log
line naming the fight.

**What remains is that no machine in reach can fail it.** QEMU's controller
publishes an extended-capability list with no Legacy Support capability in it
(`xECP=0x8`, measured), and nothing owns the controller once OVMF's USB stack
releases it at ExitBootServices. So a green `xhci_xecp_walk` certifies exactly
two things: the walk runs on a real controller and terminates, and it runs
*before* HCRST rather than after. Both halves of the interesting behaviour —
firmware that holds the semaphore, and firmware with SMI-on-OS-ownership armed
— are first observed on the T14 or not at all.

The untrusted-input half is testable and is tested, because it needed no
hardware: `xhci-xecp-selftest` walks eight synthetic lists at init (a pointer
past the register window, a link that leaves it, a window reading all ones, a
chain of minimum-length links, ours first/last/absent) and logs how many were
refused. The walk cannot loop, for three independent reasons — the next pointer
is a strictly positive forward delta, every read is bounds-checked against the
mapped window, and the iteration count is capped at 64 — and the self-test's
teeth were shown by deleting the end-of-list check, which turned one case into
`Err(TooMany)` instead of `Ok(None)`: still bounded, still red.

Not built: the Supported Protocol capability (ID 2) is walked past. Nothing
reads it yet, so this is a gap in the parse rather than in behaviour. It was
also the leading suspect for the T14's five ports that reset and did not enable,
and it was not the cause — see `2b0631f`; the reset write is the same for both
protocols, and knowing which port is which would not have changed it. What it
*is* needed for is the entry below.

### A port that fails its reset gets no second try, and no warm reset

xHCI 1.2 §4.19.5: "Only a USB3 protocol port may fail the bus reset sequence.
USB2 protocol ports never fail." A USB3 port that does fail comes back with
PLS = RxDetect, PRC set, the speed field zero and **CCS cleared** — so the
failure is distinguishable from success at the register, and `init_device`
distinguishes nothing: it checks PED, logs `reset but not enabled` and drops the
port for the life of the boot.

The spec's answer is §4.19.5.1, a Warm Port Reset: software writes WPR (bit 31)
instead of PR, which resets the USB3 link itself rather than only the device.
This driver never writes WPR, and `PORTSC_RWS` deliberately excludes it, so
there is no path to one. Linux retries either way — `PORT_RESET_TRIES` is 5 and
`PORT_INIT_TRIES` 4 in `drivers/usb/core/hub.c`, and `hub_port_reset` escalates
a failed hot reset to a warm one.

Doing this properly needs the Supported Protocol capability above, because
"retry as a warm reset" is only correct on a USB3 port and WPR is RsvdZ on a
USB2 one. It costs a device on a receptacle whose link does not train first
time; on the T14 the receptacle in question is the one the boot stick is in.
Nothing in QEMU can fail a reset — `xhci_port_reset` sets PED for every speed it
knows and never takes the failure path — so this needs an actuator of its own,
and `xhci-portsc-rw1c`'s shape (replace what the register reads) is the one that
fits.

### First-match device selection that remains, and why

`pci::enumerate` returns every function now, so a driver taking the first match
does so visibly. Two do, and both are deliberate:

- **NVMe.** `nvme::init` takes the first class-0108 controller. A machine with
  two NVMe drives loses the second, and there is nowhere to put it:
  `page_cache::init` takes a single `Box<dyn BlockDevice>`. Making this an
  enumerate-all is a storage-stack change, not a PCI one.
- **The four virtio drivers.** Each takes the first device with its
  (vendor, device) pair. A second NIC or a second GPU would be dropped. These
  are QEMU-only devices — no virtio function appears on the T14 — so the
  exposure is a test-shape one, and no profile declares two of anything virtio.

Neither is a defect today. Both become one the moment a second such device is
reachable, and the enumerate-all they would need now exists.

### FOLLOW-UP — the xHCI driver's waits are spins with preemption disabled, wherever they run

`bdf2596` moved the *boundary* — an input read no longer drives the driver — so
the only thread that runs enumeration and recovery now is the one inside
`drain_irqs`. That fixes who pays; it does not change what is paid.

Every wait in this driver is a spin against a wall-clock deadline, taken while
holding `XHCI`, which is a ticket spinlock and therefore preemption off for its
whole life:

- `settles()` — controller halt, HCRST, CNR, R/S, and the port reset. Bound
  `USB_TIMEOUT_NS`, 2 s.
- `wait_command()` and `wait_transfer()` — every command and every transfer.
  Same bound, and an endpoint recovery issues up to three of them in a row
  (Reset Endpoint, Set TR Dequeue, CLEAR_FEATURE(HALT)).

So a worst case is a CPU that does not reschedule for **six seconds**, and an
ordinary hot-plug enumeration on the T14 is ~14 ms of it (the entry below).
Nothing in the suite can measure the bad case: QEMU answers every one of these
in microseconds, which is why a driver built entirely out of them passed
everything here for a season.

**The conversion is the same idiom `PortWork` already uses** — the debounce and
the port reset were spins until #94 and are now states the poll returns to — so
the shape is known and the work is mechanical rather than novel. What makes it
big is its extent: `configure` is a straight line of control transfers and
`restart_endpoint` a straight line of commands, and each has to become a state
machine that gives the pass back between steps. That is the whole enumeration
and recovery path, which is why it is filed rather than folded into #116/#118.

One case is *not* fixed by that and needs its own answer: `storage_read` and
`storage_write` are called by the page cache on a faulting thread, so a thread
touching a file on a USB disk drives a SCSI command under the same lock. The
input poll was gratuitous and could simply be deleted; this one is inherent, and
the choice is between an I/O thread and making the block layer asynchronous.

### The hotplug enumeration blocks a scheduler pass, and its debounce keeps a CPU awake

Both are the price of `poll_if_pending` being the only context the driver has,
and both are bounded and paid only by a machine somebody has just plugged into.

**The enumeration.** `device::configure` runs inline: Enable Slot, Address
Device, three or four control transfers, Configure Endpoint. Under TCG it is
microseconds — the whole hotplug sequence in `xhci_hotplug` is inside one
millisecond of guest time — so nothing in the suite can measure the real cost.
The one hardware figure there is says the T14's five boot-time devices took
346 ms including 5×55 ms of port reset, so roughly **14 ms each** for everything
`configure` does (`specs/metal-hardware-inventory.md`). That is a scheduler pass
of that length on the CPU that services the plug, with preemption disabled under
the `XHCI` lock — the same order as `log_file`'s flush, which §10 measures at
2.0–9.7 ms and calls out for the same reason. The port reset was the dominant
term and is already out of it; taking the rest out means a state machine over
the control transfers, which is the whole enumeration path rewritten.

**The debounce.** `PORT_WORK_AT` keeps a CPU with nothing to run out of `hlt`
until the port's deadline, because nothing else would bring it back: the connect
edge was the last interrupt the controller had to give, and the scheduler arms
its one-shot for parked *tasks*. It is a deadline rather than a flag, so the
`XHCI` lock is taken once when it expires and not by every CPU on every pass for
the length of it — but every *idle* CPU declines to halt for the interval, which
is 100 ms for an ordinary plug and up to the 2 s transfer deadline behind a port
that will not reset. Power, never latency: `Action::Idle` is reached only when
there is nothing runnable, and this decides whether to sleep and nothing else.

What would remove both is a way for a driver to ask the scheduler for a deferred
callback at a deadline — which is also what `i8042::verdict_due` and
`log_ring::file_has_pending` are working around in the same condition. That is a
scheduler-core addition and wants the owner's sign-off.

### CLOSED — a HID interrupt completion the controller did not like stopped that device for good

`dispatch_event` requeued a bound HID device's interrupt TRB only for completion
codes 1 (Success) and 13 (Short Packet). Every other code — a stall on the
interrupt endpoint, a transaction error, a babble — was dropped where it was
read: no requeue, no log line, no fault, and that endpoint carries exactly one
TRB, so the device was silent for the rest of the boot with every bind-time line
reading perfectly.

**Recorded as a residual while hotplug was wired, and it bit the owner the next
day.** A Logitech mouse (`vendor=046d product=c077`, `speed=2`, so low speed)
hot-plugged into the T14 bound flawlessly and delivered nothing:

```
[kernel 30.485 cpu0] xHCI: USB mouse ready on slot 6, int_ring +0x5f000
[kernel 30.485 cpu0] xHCI: pointer on slot 6 merges as source 1
[kernel 58.659 cpu0] xHCI: port 1 disconnected
```

28 seconds, no motion, nothing in between. The log cannot name the completion
code, because the driver threw it away — which is the same defect one level up
and the reason the named line below is as much of the fix as the recovery is.

An unexpected code is now recorded on the device and acted on by
`recover_endpoints`, which logs the device, the endpoint and the code (named
where xHCI 1.2 Table 6-90 names it, at every line in the driver that prints
one). The recovery is `restart_endpoint`, moved out of `msc.rs` unchanged:
which command is legal is a property of the Endpoint State in the controller's
output context and nothing about that is per class.

**Recorded rather than recovered at the point the code is read**, because
`dispatch_event` runs inside `wait_command` and `wait_transfer`, which are both
draining the same ring for a caller waiting on one particular event. A recovery
issued from there submits commands and waits on that ring itself, and the events
it consumed would include its caller's — a disk's data phase disappearing
because a mouse stalled. `poll` and the end of the boot scan are the two places
nobody else is waiting, and the second is not optional: an endpoint holding no
TRB raises no further interrupt, so a device whose *first* transfer failed
during the scan would otherwise stay recorded and silent for the whole boot.

Repeated-failure policy: `MAX_HID_FAILURES` is 8 consecutive failures, cleared
by a delivered report, so a device that glitches once is never let go for it and
one that fails every transfer is let go on its own service interval rather than
costing two commands and an event-ring spin per poll inside a scheduler pass.
What the caller sees is `let_go` — the device named, its keys or its
button-table entry given back, its slot disabled, and the port left *marked
attached*, because a port whose `attached` goes false with the device still in
it reads as a fresh connect and the driver would enumerate the same endpoint
again every debounce. Unplugging is what clears it, which is what the line says.

**Gate `xhci_hid_break`, both timings, negative-controlled twice.** The actuator
is a kernel feature (`xhci-hid-break-first`, `xhci-hid-break-late`): QEMU's
`usb_hid_handle_data` answers an IN token on endpoint 1 with a report or with
NAK and has no path to `USB_RET_STALL` for it. It replaces the completion code
*and the report that transfer delivered*, which is what stops the gate being
vacuous — QEMU really moved a mouse report into the buffer, and a driver that
dispatched it anyway would publish a delta it never earned.

- Fixed, first-completion boot: no `mev` line precedes the break line at all,
  `a` never arrives while `b` and `c` do, and `hello` plus a `(2560, -1920)`
  delta cross both endpoints after the recovery. One of ten pointer moves is
  lost, exactly as a failed transfer loses it.
- Negative control 1, `dispatch_event` reverted to the pre-fix drop: `input done
  keys=0 pointer=0`, zero `mev`, zero `kev`, zero recovery lines. Both devices
  go silent from their first completion — the T14's picture.
- Negative control 2, recovery kept but the requeue removed: both endpoints
  named their code and both were found Running and restarted, and still `input
  done keys=0 pointer=0` with zero `mev` and zero `kev`. The gate reaches the
  requeue, not just the log line.

### The T14's mouse may not have been this defect at all, and the next boot is what says

Fixed in passing and **unverifiable in this suite by construction**, so it is
recorded rather than claimed. The HID endpoint context's dword 4 was a flat `8`
copied from EP0's, where a control endpoint has no Max ESIT Payload and 8 is a
setup stage's Average TRB Length. Every periodic endpoint this driver configured
therefore declared that it moves **zero bytes per service interval** — the term
xHCI 1.2 §6.2.3.8 defines and §4.14.2 makes the periodic scheduler's input.
Linux's `xhci_endpoint_init` writes `max_packet` into both halves for a low- or
full-speed interrupt endpoint; the driver now does the same. QEMU has no
bandwidth scheduler and never reads the field, so no test here can tell the two
values apart.

That leaves two candidates for the 28 silent seconds, and they are
**distinguishable on the next metal boot**, which is why closing the first did
not close this:

1. the endpoint's first transfer completed with an error — the new line names
   the device, the endpoint and the code, and the recovery runs;
2. the endpoint was never scheduled at all — **no line, because no completion
   event ever arrives**, and the mouse is still silent.

Ruled out already: SET_PROTOCOL is sent to every boot-interface HID and the T14
log carries no failure line for it, so EP0 was not left halted (see the open
item on that in this section). The interval encoding is legal —
`bInterval=10` frames at low speed gives `log2(10 × 8) = 6`, inside Table 6-12's
3..10. `SET_IDLE` (HID 1.11 §7.2.4) is the one class request the enumeration
path does **not** send, where Linux's `usbhid_parse` sends it unconditionally
and ignores the result; its absence leaves the device on its default idle rate,
which is chattier and not silent, so it is not a candidate for this — but a
device that expects it is a real class of hardware and nothing here has one.

### OPEN — the T14 lost every integrated input at 6.6 s, and the log cannot yet say why

All three integrated pointers and the keyboard are behind the one i8042, and all
three went dead 6.6 s into the 2026-08-03 compositor session. The whole of what
the driver said about it, and the last `i8042:` line in a 58-second log:

```
[kernel 6.594 cpu0] i8042: 1 interrupts and 1 bytes, nothing decoded — no event from [aux 0x08], first seen at 6594ms
[kernel 6.609 cpu1] i8042: the pin asserts — 6 interrupts, 6 bytes, 0 keys, 2 motion, no event from [aux 0x08, aux 0x06, aux 0x08, aux 0x0e], first seen at 6594ms
```

**That line does not say what it looks like it says, and the first task on it was
opened on the strength of the misreading.** `0x06` has bit 3 clear and no packet
head ever does, so the four listed bytes read as a framer that had lost the
frame. They are not. Six bytes, two motion events, four bytes named: 2 × 3 = 6,
and the four are the head and first body byte of two whole, correctly framed
packets — `0x08` is a resting head and `0x06` is a `dx` of +6. **The pointer was
framing perfectly.** The arithmetic is forced and no reader would do it.

Closed, therefore: the decoder did not desync, and no fix for a desync was
needed. What was wrong is the instrument, and it is now fixed (`647c3c0`,
`toyos-ps2`) — `MouseOutcome` could not distinguish a byte held inside a packet
from a byte thrown away at a boundary, so two of every three bytes of a healthy
pointer stream were reported as suspects. `i8042_mouse` now runs 3018 bytes of
clean packets and requires the driver to name none of them; reverting the split
reds it with the T14's own line shape.

**What remains open is the actual question, and the log cannot answer it.**
`IRQS` counts in the ISR before any decoding, so 6 interrupts is hardware truth
— but it is truth *as of 6.609 s*, which is when the driver stopped speaking.
`HEALTH_DONE` was terminal. For the remaining 54 s the log cannot separate:

- **the pin stopped asserting** — a wedged controller, a lost edge, an EC that
  stopped scanning, an RTE that got masked; from
- **bytes kept arriving and decoded to nothing** — a wire-format or framing
  fault, in this driver.

Those are opposite defects in opposite subsystems and the counters that tell
them apart were read once. Two facts are established and neither settles it: all
six bytes were aux (four named `aux`, two produced motion, `0 keys`), and **the
keyboard produced no byte at all in 58 s** — not "stopped at 6.6 s", never. The
same machine's earlier boots drove a shell off that keyboard (`metal-hardware-
inventory.md`), so it is not a routing fault.

The cadence fix is what makes the next session decisive rather than a guess:
after the verdict the counters repeat, at most once per 10 s and **only when the
pin has asserted since the last line**. That gating is the point — past the
first repeat, no line means no interrupt, so silence becomes evidence instead of
absence of evidence. `i8042_health_cadence` gates it, and reverting either half
(fire on the timer, or make `HEALTH_DONE` terminal again) reds it at 9 lines and
0 lines respectively against the required 2.

**What the next boot should capture.** A repeat line dated after 6.6 s, or none.
If bytes are arriving, `undecoded`/`discarded` name the fault in this driver. If
no line appears at all, the pin is not asserting and the next suspect is the
controller or the EC — and nothing in `toyos-ps2` can be responsible.

Two things deliberately not concluded:

- **The touchpad is not evidence of a mux problem.** The T14's touchpad is
  I2C-HID off an LPSS controller that is not on the PCI bus at all; the EC
  mirrors it onto the aux port beside the TrackPoint. The aux device answered
  `0xF2` with id `0x00` — a plain 3-byte mouse — so the driver's 3-byte frame is
  what the wire carries, and the 4-byte IntelliMouse mismatch usually blamed for
  a PS/2 desync is not available here.
- **The USB mouse plugged in at 30.4 s produced no motion either**, which is the
  xHCI HID completion-requeue item in this section and not this one.

### PARTLY CLOSED — the i8042's one diagnostic line could not be read on the machine it is for

The T14 booted from `target/bootable-diet.img` (sha256
`9bda620d…e531aa`, the file still on disk and re-hashed) and reached the
compositor with the integrated keyboard and the TrackPoint dead. The driver's
entire contingency for that — `specs/metal-boot-plan.md` M2, the pre-flash
gate's "what this gate does NOT cover" item 1, and `1bf5f61`'s commit message —
is **"one loud line on the laptop's own screen instead of a bisect"**. That line
is not readable, and this is the defect that made the first metal input attempt
uninterpretable.

`panic_console::boot_checkpoint` returns immediately once
`SCREEN_OWNED_BY_USERLAND` is set (`panic_console/mod.rs:478`), and
`device::try_claim(DEVICE_FRAMEBUFFER)` sets it (`device.rs:83`) as the
compositor's third statement (`compositor/src/main.rs:719`). So the last
kernel screenful ever painted is the one at `Boot: complete`, and the compositor
overwrites it with the desktop a few tens of milliseconds later. Measured on
`cargo test --test toyos-build -- metal_sim --nocapture`, the
`metal_sim_compositor` boot: the three `i8042:` lines at 0.099–0.100 s,
`Boot: complete (196ms)`, and the compositor's own first console line after the
daemon-exit lines at 0.244 s. **The screen carrying the answer is up for well
under a fifth of a second and there is no key that pauses it** — `page_forever`
is reached only from `halt_all_cpus`, so a *successful* boot never pages.

The content is there, which is the frustrating part: 26 kernel log lines
separate the last `i8042:` line from `Boot: complete` in that run, against 67
text rows on a 1920x1080 panel, and the longest line in the range is 158
characters against 240 columns — so the line is on the final boot screen, just
not for long enough to read or photograph by hand.

Consequences, in the order they bite:

- **Every one of the driver's seventeen refusal paths is silent in practice.**
  `i8042::init` has sixteen `return`s that each log one line, plus a success
  line whose tail reads `MASKED` when the unmask failed. On the flashed
  configuration all of them look identical from the owner's chair: a desktop
  with dead input.
- **A keyboard-side refusal also costs the pointer.** Every `return` in the
  keyboard block (`i8042/mod.rs:1015-1075` — `0xF5`, the `0xF0 0x00` read-back's
  five refusing arms, `0xF4`) happens *before* the aux block at `:1077`, so the
  TrackPoint is never initialised either. "Keyboard and TrackPoint both dead"
  therefore discriminates nothing — it is the signature of every failure mode,
  including the ones that are purely keyboard-side. The T14's own first answer
  was one of these, and it is no longer among them: a keyboard that will not
  report its scancode set now attaches on firmware's translate bit and the aux
  block runs, which `i8042_kbd_echo` asserts. The other refusals are unchanged.
- **The intended reading of a dead touchpad is destroyed.** The gate told the
  owner a dead touchpad is expected (I2C-HID, unbuilt) and a keyboard refusal is
  the driver working. Neither statement is checkable without the line.

**Built, as `--diag-boot`.** `diag/system.toml` plus a flag on the build system,
the way `--gop` and `--metal-sim` are flags: it writes `target/bootable-diag.img`
instead of `bootable.img`, so no edit to the shared `system.toml` and no image
left contradicting the committed config. The guarantee is structural rather than
a property of the init list — the compositor is the only process that claims the
framebuffer and it is not built into the image at all — and the kernel and
bootloader binaries are unchanged by the flag, so what the owner reads off a diag
boot is what the shipping kernel does. Gated by `screen_diag_boot`
(`tests/toyos.rs`, in `SCREEN_TESTS`): boots the same config on `Profile::Metal`,
polls until the last checkpoint has painted, holds five seconds, and asserts an
`i8042:` line and `Boot: complete` are still decodable. Teeth: with
`/bin/compositor` put back into the init list the fill check reds on
`[24, 24, 37]` against the checkpoint's `[0, 0, 0]`, and the decoded desktop
carries zero occurrences of either asserted string.

Three things it does **not** give, in the order they will bite:

- **Almost nothing after `Boot: complete` is visible.** The last checkpoint is
  otherwise the last paint on a successful boot, so a daemon that dies later is
  exactly as silent as before. The mode answers "how far did the kernel get and
  what did it say", which is the i8042 question, and nothing else.

  **`--console-boot` is the other half and does not replace this one.**
  `/bin/console` claims the framebuffer, seeds its scrollback from
  `/log/kernel.log` so the boot log survives the claim, and puts a shell
  underneath — so anything after `Boot: complete` is one typed command away.
  What it cannot do is what diag exists for: claiming the screen is exactly
  what stops `boot_checkpoint` painting, so a machine that wedges *before*
  userland shows nothing at all in that mode. Two images, two questions.

  Its own residuals: the seed is read once at startup, because the console
  copies the shell's output to its own stdout and that is the ring `log_file`
  drains — a tail would feed itself; and it needs `/log`, which
  `fat32_adapter::mount` gives only to a machine that booted from USB (below), so
  on anything else the console starts with one line saying the log is not there.

  The one exception is deliberate and is the i8042's own health verdict
  (`d13efa6`). The driver now says once whether the pin it armed has ever
  asserted — a quiet verdict emitted from the first scheduler pass that finds a
  CPU with nothing left to run, and an alive line emitted by the pass the first
  interrupt itself schedules — and repaints the panel through
  `boot_checkpoint` for each, *only* on a machine with no console at all
  (`serial::has_console()`, the same predicate `panic_flush` refuses on). On a
  diag boot that turns the dead-input question into an interaction: the frozen
  screen ends in `armed at 106ms, idle at 221ms, 0 interrupts — the pin has
  never asserted`, the owner presses a key, and either the screen repaints with
  `the pin asserts — N interrupts, N bytes, N keys` or it does not move.
  `screen_i8042_health` is the gate, on a muted metal-sim guest; its teeth are
  a `to_screen` that returns immediately (the line is in the ring and not on the
  glass) and a `verdict_due` that never arms (nothing to paint).

  **It does not reach the shipping image.** `boot_checkpoint` still paints
  nothing once the compositor claims the framebuffer, so on `bootable.img` both
  lines reach the log ring and stop there. The health *signal* is the fix; the
  *surface* is still the open problem this entry is about, and the durable
  answer is a log sink that survives userland — the USB-storage/FAT32/GPT work,
  not another boot mode.
- **The T14 pages, and only the footer says so.** Measured on the shipped image's
  own log: 75 display rows at the panel's 240 columns against a 67-row grid, so
  `pagination` gives two pages and the checkpoint paints `[page 2/2]` with the
  newest 66 rows — the first nine rows of the log are above the window. The first
  `i8042:` line is 19 rows above the end, so it is on that page with room to
  spare. QEMU's stdvga grid is 96x256 and the same log is 74 rows there, i.e. one
  page and no footer, so **the footer branch of `screen_diag_boot` has never
  executed**: it is a guard, not a certification, and the machine that will
  exercise it is the laptop.
- **`kernel/src/main.rs:463` asserts a non-empty init list**, so "spawn nothing"
  is not available; a violated assert would paint a panic report instead of a
  boot log. The list is therefore the least a program in this tree can do,
  `/bin/toybox pwd`. It used to be the shipping list's own first entry,
  `locale --load`, which went with the layout syscall — and the shipping list
  now begins with the compositor, which is the one process this image must not
  contain.
- **Every em-dash in a kernel log line is three dots on the panel.** `font8x16`
  holds codepoints 0x20..=0x7E and `draw_glyph` maps everything else to `.`
  (`panic_console/mod.rs:778`), so a 3-byte UTF-8 `—` renders as `...` and costs
  three columns instead of one. Measured on `screen_i8042_health`'s decoded
  screen: `0 interrupts ... the pin has never asserted`. 44 of the kernel's 448
  `log!` sites contain one, and the i8042's diagnostic lines are among the
  densest. Cosmetic on its own; it is not cosmetic against the T14's 240-column
  wrap, which is what decides whether a line is one display row or two, and
  therefore whether it is on the page the checkpoint paints. Cheapest fix is to
  render the three-byte sequence as a single `-`; the honest one is to stop
  putting non-ASCII in `log!`.

`specs/metal-log-capture.md` is the durable version of the same problem and its
Phase 2 fixed the *panic* half only.

### The pre-flash gate certified everything except the milestone

`specs/pre-flash-gate.md` §7 records **GO** at `b82fc4a` with a 182/182 guest
suite. Its six sections are storage safety, image well-formedness, boot-time
panics, the on-screen console, and two sections of "recent changes do not alter
boot". **There is no input section**, and the seventeen-row verdict table has no
input row. Input — the thing M2 exists for and the reason the stick was flashed
— appears only as items 1 and 2 of "What this gate does NOT cover".

That is the hole, and it is not "the gate ran the wrong test". The gate's own
method is to ask a false-pass question per item, and it asks it well for the two
items whose QEMU-versus-hardware divergence it noticed: §3.2 (TCG always reports
FSGSBASE) and §3.3 (QEMU's `stride == width`), both explicitly recorded as
read-verified because QEMU cannot exercise them. The i8042 has **more** such
branches than either, every one of them silent (above), and no item asks about
any of them.

What was actually established, and what was not:

- `metal_sim_input` is a real test and it passes: `cargo test --test toyos-build --
  metal_sim` is 3/3 in 15.7 s, `metal_sim_input` in 9 s. Its guest program
  (`tests/toyos-rust-tests/src/bin/input_events.rs`) prints only bytes it read
  from the two device fds; the assertions are `typed.contains("hello")` and an
  exact `(DX*scale_x, DY*scale_y)` delta with the scale read out of the kernel's
  own boot line; and `metal_sim_argv_check` rules out the classic false pass
  (QEMU routing injected input to a USB HID handler). It certifies i8042
  → userland delivery on QEMU's i8042 and nothing about Lenovo's EC.
- **Its teeth were never re-proved after the rewrite.** `0977c8c` records three
  negative demonstrations (`i8042::init` returning immediately, the aux port
  never enabled, the keyboard GSI never unmasked) — all of them against the
  *pixel* version, which `efbeed7` deleted the same day and replaced with the
  event-parsing version. `efbeed7`'s message proves teeth for
  `screen_late_panic` and not for the new `metal_sim_input`. Nothing suggests it
  is vacuous; it has simply never been shown red.
- **The second artifact, built for the FADT-gate removal.**
  `target/bootable-diag-3f110ad.img`, 35,753,984 bytes, sha256
  `1f3eac841ec343a7f5ad69a9f5964a21d79b2f5e763242ef013bad871eeec3b3`. Built by
  `build::build(.., Boot::Diag)` from a detached worktree at `3f110ad` with a
  clean `git status --ignore-submodules=all`, so none of the five agents'
  uncommitted work is in it; `rust/`, `toyos-ld/target` and `toyos-cc/target`
  symlinked to the main checkout, and a throwaway `src/bin` driver rather than
  `cargo run`, because `toolchain::ensure` re-links the shared rustup toolchain
  from any other root. Its initrd holds exactly one file (`bin/toybox`,
  2,140,152 bytes); the strings `i8042: fault injection armed`,
  `i8042: drain bytes=`, `test-late-panic` and `test-runner` are absent, so it is
  the plain default-feature kernel. Booted headless on the metal-sim shape before
  being handed over: the four `i8042:` lines print, `Boot: complete (234ms)`,
  toybox exits, nothing repaints after.
- The flashed kernel is the tested kernel. `target/bootable-diet.img` contains
  `i8042: kbd set2+xlat` and `i8042: absent (FADT rev ` and does **not** contain
  `i8042: fault injection armed`, `i8042: drain bytes=`, `test-late-panic` or
  `debug-wait`, so it is the plain default-feature kernel that `metal_sim_input`
  boots (`BootOptions::default()` is `kernel_features: &[]`; `src/build.rs:405`
  passes none for a non-debug `--build-only`). The root init string is present
  exactly once and `test-runner` and `librustc_driver` not at all.
- **Two shape dimensions the harness never varies.** Every `BootOptions` defaults
  to `smp: 2` and no input test overrides it; the T14's own boot line reads
  `MADT cpus=[0, 2, 4, 6, 1, 3, 5, 7]`. And all six tests that inject i8042
  input drive a guest that busy-polls `read_nonblock`
  (`i8042_keyboard.rs`, `input_events.rs`); none blocks in `sys_read` or in
  `Poller::wait`, which is what the compositor — the flashed machine's only
  consumer — actually does. The wake path itself is shared with the xHCI HID
  path from `sched/driver.rs:drain_irqs` onward and is exercised by every
  usb-kbd boot, so this is a coverage gap rather than a suspected defect.
- The interrupt topology is the one hardware risk that can be **downgraded**
  rather than assumed, from the T14's own first-boot photograph (`first-boot.jpg`,
  `0e267bb`): `ioapic: id=2 at 0xfec00000 ver=0x20 gsi 0..119 masked 120/120` and
  `ioapic: iso bus:irq->gsi [0:0->2 edge/high, 0:9->9 level/high]`. No override
  covers IRQ 1 or IRQ 12, so `gsi_for_isa_irq` returns identity/edge/high exactly
  as under QEMU; the unit covers both GSIs; and 120/120 masked read-backs prove
  the MMIO window is a real redirection table. `route`'s destination check is
  satisfied by the BSP's `LAPIC: x2APIC enabled (ID 0)`.

### CLOSED — the T14's FADT denies its own 8042, and the gate believed it

The laptop's first `--diag-boot` printed one line and stopped:
`i8042: absent (FADT rev 6 iapc_boot_arch=0x0011)`. The checksum passed, so that
is firmware speaking rather than an unreadable table, and `0x0011` decodes as
`LEGACY_DEVICES` set, **8042 clear**, `NO_ASPM` set (ACPICA `actbl.h`). The
driver refused on bit 1 and never touched the controller; the keyboard and the
TrackPoint were never asked.

Fixed by deleting the gate, not by relaxing it. **The next boot answered the
residual, and firmware's bit was wrong**: `i8042: ok selftest=0x55
cfg=0x77->0x64 port1=ok port2=ok` — a real, healthy controller on a machine
whose FADT denies it. That boot then stopped at the fifteenth refusal, which has
its own entry below.

Two things the QEMU gates do *not* cover, both structural:

- **QEMU cannot make the FADT bit and the hardware disagree.** It derives the
  bit by resolving `TYPE_I8042` in the QOM tree, so `-machine q35,i8042=off`
  clears the bit *and* removes the device, and `-device i8042` restores both.
  `i8042_fadt_denial` therefore uses a kernel feature to substitute the T14's
  own answer, which tests the driver's response to the value and says nothing
  about the parse that produced it.
- **`absent — port 0x64 reads 0xff` is what QEMU's no-controller machine
  produces, and the T14 may not.** A machine that traps 0x60/0x64 in SMM for USB
  legacy emulation returns whatever the SMI handler emulates, so the floating-bus
  test is the *cheap* answer, not the complete one. The xHCI USBLEGSUP handoff
  runs immediately before `i8042::init` and clears the controller's SMI enables,
  which is the reason to expect the trap to be disarmed by then — argued, never
  observed.

### The T14's keyboard will not report its scancode set, and one byte reached no event

The boot after the FADT gate came out reached the keyboard and stopped one step
from the end:

```
i8042: ok selftest=0x55 cfg=0x77->0x64 port1=ok port2=ok
i8042: kbd cmd 0x02 answered Some(238), not ack
i8042: kbd refused scancode set 2 ... disabled
```

238 is `0xEE`, ECHO's own reply, returned for the **argument** of `0xF0 0x02`
after the command byte was acked and after `0xF5` had been acked — so the EC
answers commands and does not implement this one. The driver now reads the set
rather than writing it, and where the read is refused it decides the wire format
from the translate bit firmware left in the config byte (`0x77` on this
machine), which is exactly what Linux's `i8042.c`/`atkbd` do and all they do on
a portable device. `i8042_kbd_echo` gates it.

**The boot after that one worked**, and it is the first time any of this has run
on the metal it was written for:

```
i8042: kbd set2+xlat (assumed, the set query was refused) scanning on, GSI 1 -> vec 0x24 apic 0 on
i8042: aux rate=100 res=8/mm, GSI 12 -> vec 0x24 apic 0
i8042: armed at 1460ms, idle at 3394ms, 0 interrupts ... the pin has never asserted (kbd GSI 1, aux GSI 12)
i8042: the pin asserts ... 1 interrupts, 1 bytes, 0 keys, 0 motion, first seen at 11375ms
```

The driver attaches; the **aux port initialises fully** — `rate=100 res=8/mm`
means the TrackPoint answered its whole reset/id/rate/resolution sequence, which
no previous boot reached because every keyboard-side refusal returns before that
block; and a physical keypress at 11375 ms raised a real interrupt on GSI 1, so
the routing, the RTE programming, the vector and the unmask are all correct on
Tiger Lake silicon. `Boot: peripherals ready` went from 6 ms to 398 ms in the
same boot: that is the aux reset stage now actually running against a device
that takes real time, not a regression.

What is open:

- **One byte reached the kernel and produced no event, and the counters could
  not say which byte.** That is the open item, and half of it is the
  instrument. Enumerated against the real tables (`toyos-ps2/src/key.rs`),
  **84 of the 256 single byte values decode to nothing** under set 1: both
  prefixes (`0xE0`, `0xE1`), the two `Lost` codes, and every unmapped slot —
  `0x54`, `0x55`, `0x59`–`0x80` and their break forms. `handle_key` drops a
  break for a usage nothing held, which adds `0xAA` (left Shift's break under
  translation, and a keyboard announcing a reset). So `1 bytes, 0 keys` covers
  an extended key where **nothing is wrong**, a late `0xFA`, another `0xEE`, a
  device reset, and a wire carrying raw set 2 — where Enter is `0x5A`,
  Backspace `0x66`, Escape `0x76` and 23 such codes land on unmapped slots.
  Only the byte separates them. The driver now records the bytes that produced
  no event and names them in the health line, and says it a second time if a
  later byte does decode; `i8042_undecoded_bytes` gates both. The next diag
  boot answers this in one line without a reflash.
- **The wire format is still the `assumed` one, so a raw-set-2 wire is among
  the suspects.** It is not the likeliest — a mismatch would usually produce
  *wrong* events rather than none, since most set-2 codes do land on a mapped
  set-1 slot — but 23 of them do not, and the byte value is what settles it.
- **The fallback's evidence is firmware's intent, not a read-back.** `before &
  CFG_TRANSLATE` says firmware enabled a set2→set1 translator; that it did so
  for a device emitting set 2 is inference, tight but inference. The success
  line says `(assumed, the set query was refused)` rather than `(readback
  0x41)` precisely so the panel does not claim otherwise. A machine where the
  inference is wrong types nonsense, which is the outcome the read-back exists
  to prevent — there is no third instrument for it on this wire.
- **`0xF2` is not that instrument.** A translating controller answers the MF2 id
  `AB 83` as `AB 41` (`translate_table[0x83] == 0x41`, QEMU `hw/input/ps2.c`;
  QEMU's own keyboard hardcodes the same pair), which would prove the translator
  is live on the data path and not merely enabled in a bit. It is not sent,
  because Linux's `atkbd_skip_getid` withholds `0xF2` from every translated
  portable device — "on many modern laptops ATKBD_CMD_GETID may cause problems"
  — and the T14 is one. Sending a command Linux avoids on this exact machine
  class to shore up an inference is the wrong trade.

- PCID + INVPCID codepaths untested on real hardware — QEMU TCG supports
  neither. Both are CPUID-gated, so TCG falls back to a CR3 reload. Needs KVM or
  bare metal.
- TLB shootdowns still IPI all CPUs for a full flush. Per-page targeted
  shootdowns not implemented.
- The LAPIC timer uses one-shot mode; it should use TSC deadline mode
  (`IA32_TSC_DEADLINE` MSR) for precise absolute-time wakeups. The TSC is already
  calibrated for `nanos_since_boot()`.

---

### Tearing is what GOP cannot give back, and two clients still present whole

Recorded from the compositor's rendering pass (task #132), which moved
composition into system RAM and gave `MSG_PRESENT` a damage rect. Three
residuals, none of them fixed there:

**The scanout blit is not synchronised to the panel.** Nothing composes onto
the scanout any more, so a frame is never seen half-composed; what remains is
that the blit of a damage rect can land while the panel is reading those rows,
so that rect can tear. The window shrank from "a whole composite" — a wallpaper
blit, then every window, then the taskbar, then the cursor, each a separate
pass over the mapping — to one `memcpy` of the damaged rows. Closing it needs
to know where the beam is, and GOP does not say: there is no vblank interrupt,
no scanline register and no page flip in the protocol. It is the display
driver's to close, and the owner's ruling is that the driver comes later and
whole. Double-buffering does not help without the flip either — with one
scanout and no way to swap it, a second buffer is what the back buffer already
is.

**Every window client except the terminal presents its whole window.** `paint`,
`files`, `editor`, `filepicker`, and everything on winit/softbuffer (doom,
snake) draw into their shared buffer through a readable `Framebuffer` and keep
no record of where, so `Window::present()`'s "all of it" is the honest answer
for them and `present_damage` is unused. The terminal is the one that composes
through a `window::Screen` and can therefore be asked what it painted. Each of
the others is the same shape of change: compose through `Screen`, hand
`take_damage()` to `present_damage`.

**The back buffer is one screen of RAM charged to nobody.** 8.29 MiB at
1920x1080, on top of the window buffers §1's window-cap note already covers.
Same root: there is no per-process memory limit, no pressure signal and no OOM
killer, so a compositor's own working set is bounded by nothing but its code.

## 9. toyos-fat32

The crate is new (`toyos-fat32/`, host tests: `cargo test` inside it). Its
kernel adapter is `kernel/src/fat32_adapter.rs`; §10 carries what that found.
Nothing below is a defect found later — these are the residuals its own gate
identified while it was being written, recorded so the adapter's author did not
have to rediscover them.

### A cyclic chain under a *file* is bounded, not detected

`fat.rs::advance` walks a chain by a step count derived from something the
chain cannot influence — a file's size field, `MAX_DIR_ENTRIES`, the volume's
cluster count — so a cycle costs a bounded number of FAT reads and never a
hang or an unbounded allocation. A self-loop (`c → c`) is rejected because the
comparison is free. A longer cycle under a file is not: the read returns that
file's own earlier bytes again, within its declared size.

Detecting it on the read path needs either a tortoise-and-hare, which doubles
the FAT reads on every sequential access, or a full walk from the head at open
time — and the second is incompatible with the position hint that makes
sequential access O(1) rather than O(n²) in the first place.

**The write path does detect it, and this entry used to claim more than was
true.** The original wording said the damaging cases were all covered by
`free_chain`, `chain_len` and `chain_last`. `free_chain`'s cycle detection is
"a revisited cluster reads as free" — it needs the walk to *revisit*, and
`truncate_chain` writes an end-of-chain marker at the cluster it is keeping,
which is an exit the walk takes instead. The audit
(`specs/type-safety-audit/storage-stack.md` F3) demonstrated `set_len`
returning `Ok(())` having freed every cluster the truncated file still needed,
with the directory entry still naming the first of them. A residual that
overstates what is detected is worse than one that admits the gap, because
nobody re-checks it.

What holds now: `truncate_chain` is preceded by `Fat32::verify_acyclic`, a
tortoise-and-hare that runs **before** anything is written, so a cyclic chain
leaves the volume untouched. It is the only cycle detection in the crate and
it is affordable exactly where the read path's is not — truncation is one
operation that already walks the whole chain, rather than one per page.
`free_chain` also takes an anchor now, which guards the last retained cluster;
that alone was not enough, because the audit's cycle closed above it.
`chain_len` and `chain_last` do bound a directory that never ends, and that
part of the original claim was correct.

Two tests pin the split: `a_longer_cycle_is_bounded_rather_than_endless` (read
path, bounded, no error) and
`truncating_a_cyclic_chain_does_not_free_the_clusters_it_keeps` (write path,
refused with nothing freed).

### `walk` cannot see an empty directory

`Fat32::walk` returns files only, with directories implied by the `/` in a
path — which is exactly the convention `vfs::FileSystem::list` expects, and
what `TmpFs` and both bcachefs adapters do. The consequence is the same one
the VFS already has: a directory with nothing in it is invisible through
`list`, and the VFS's `created_dirs` set only covers directories created in
this boot. An empty directory that was already on the ESP will not appear.
`Fat32::read_dir` answers correctly per directory; nothing calls it yet.

### `rename` refuses an existing destination

FAT gives no way to make a replacement atomic. Deleting the destination first
leaves a window in which neither name resolves, which is worse than an error
the caller can act on — so `Fat32::rename` returns `AlreadyExists`. A VFS
`rename` that wants POSIX overwrite semantics has to do the delete itself and
own that window.

### Bounds that are policy, not format

Each is a number this crate picked, with its derivation at the definition:
`MAX_DIR_ENTRIES` (65,536 entries, 2 MiB, ~20k files in one directory);
`MAX_WALK_DEPTH` (32); `MAX_SHORT_NAME_CANDIDATES` (64, after which a create
into a directory built to collide returns `NoSpace`); `MAX_LFN_CHARS` (255,
this one *is* the format). A `walk` or `read_dir` past the caller's `limit`
refuses rather than truncating, for the reason `vfs::MAX_LIST_ENTRIES` gives.

### A handle's fingerprint cannot survive a delete-and-recreate under the same name

`File` identifies its directory entry by the 8.3 field plus the five creation
stamp bytes, and `Fat32::live_entry` checks it on every `write`, `set_len` and
`flush_meta`. That catches the slot being taken by a *different* file, which is
the dangerous case and the one the audit demonstrated (F2: the guard compared
first clusters, and 0 is every unwritten file's first cluster, so a slot
refilled by another empty file matched and the stale handle rewrote the
newcomer's entry — with `fsck_msdos` calling the volume clean).

What it still cannot distinguish is a file deleted and recreated under the same
name with the same creation timestamp, because FAT has nowhere to put a
generation number. The stamp's resolution is 10 ms, so a caller that stamps
from a real clock is safe and a caller that passes a constant — every test in
this crate does — is not. The kernel adapter should hold handles for as long as
its own file objects live and no longer, rather than relying on this to be a
generation counter, which it is not.

### Two things `fsck_msdos` does not check, found by breaking the code on purpose

Recorded because it generalises past this crate: **a host validator's silence
is evidence about the validator, not only about the code.** Sixteen deliberate
breakages were run against the suite. Fourteen went red. Two did not, and
neither was harmless:

- **A stale FAT mirror.** `fsck_msdos -n` does not compare the FAT copies, and
  a mount reads only the active one — so a driver that updates FAT 0 and
  leaves FAT 1 behind passes fsck, passes a real mount, and passes every
  read-back test, while leaving a volume that reads differently the moment
  anything consults the mirror.
- **Duplicate 8.3 names.** Neither fsck nor a mount looks at short names; both
  use the long ones. Dropping short-name uniquification entirely was invisible.

Both now have a test that reads the raw bytes off the device
(`every_fat_copy_stays_in_step`, and the tail of
`colliding_short_names_stay_unique`), and both mutations go red.

Related, and the reason the gate does not read an exit code: **`fsck_msdos -n`
exits 0 while printing `Fix?` for problems it declined to repair, and exits 0
on a volume it has just declared dirty.** `common::Image::fsck` matches the
output line by line against the exact shape of a clean run instead. A gate
written the obvious way would have been green on a corrupt volume.

**The 2026-08-01 audit is the sequel to that paragraph and the sharper version
of it.** Sixteen breakages of the *code* caught fourteen; an independent
auditor attacking the *state space* instead — a file that is empty, a chain
that is cyclic, an entry that is crafted — found six more, four of them on the
write path and one that wrote 256 GiB outside the volume and returned `Ok(())`.
Every one of them was reachable through the public API on a volume this suite
already had. The lesson that generalises past FAT32: **mutating the
implementation tests the paths you wrote; it says nothing about the states you
did not think to construct.** Both are needed, and the second is the one a
green suite hides.

### CLOSED — `bcachefs::BlockIO` is infallible, so the device error channel stops one layer short

`BlockIO`'s three operations return `Result<(), DeviceError>` as of this
session, so `PageCacheBlockIO` no longer has to turn a device failure into
*something*. It used to turn it into zeros and a log line: fail-closed rather
than correct, because zeros fail bcachefs's own structural checks and a read
error therefore reached the btree wearing corruption's clothes.

`DeviceError` carries nothing; the crate-private `BlockIOExt` — which every
call site inside the crate goes through — attaches the block number, so an
error a filesystem operation returns names the block that operation asked for
and no implementation can name a different one. `FsError::{DeviceRead,
DeviceWrite, DeviceSync}` carry it outward.

Three things the ripple changed beyond signatures, each worth knowing:

- The allocator's `free_blocks`/`next_alloc` move only after the bitmap block
  is on the device. A reservation the device would not record is not a
  reservation.
- `Superblock::read` no longer falls through to the backup after a *refused*
  read of block 0 — the backup answers a bad superblock, not a bad device.
- `read_link`, `file_mtime`, `file_extents`, `file_size_meta` and `is_symlink`
  return `Result` instead of folding a device failure into "no such file".

Gates: four host tests driving a `BlockIO` that refuses one chosen block —
data, btree node, block 0, and a write. Nothing else in the tree can stage a
device failure (`VecBlockIO` cannot fail, QEMU's NVMe does not fail a read),
which is the same gap the page-cache entry below records.

**What is left, and both are real gaps:**

`vfs::FileSystem` still has no channel where `bcachefs` now does: `file_size`
and `read_link` return `Option`, `file_mtime` a bare `u64`, `delete` a `bool`.
Those sites go through one `no_channel` helper in `bcachefs_adapter.rs` that
logs and then answers "nothing here" — the sentinel moved one layer up rather
than removed. The trait is shared with `tmpfs` and `fat32_adapter`, so growing
it a channel is its own change.

`fd::try_write` has no honest error code for "the device did not do it". It
stops at the failed page and returns the short count, which is what `write`
means — but a request whose *first* page fails gets `SyscallError::Unknown`,
because none of the nine variants says this and adding one is an ABI change that
needs discussing. `BcacheFsAdapter::list` now returns the same `Unknown` for the
same reason, rather than an empty listing a caller cannot tell from an empty
directory.

### The page cache's un-index on a failed fill has no test that can fail

`PageCache::read` now unbinds the slot when the fill fails, so a slot cannot
stay labelled with a block whose read did not happen. **Measured, not
asserted**: with the `self.unbind(slot, block)` line deleted, all three USB
storage tests still pass — 3/3 green in the same session that saw them go red
for a real driver defect. Nothing in the suite drives a *failing* read through
the page cache, because the page cache's device is NVMe and QEMU's NVMe does
not fail a read.

What it would take is a fault-injection actuator on the page cache's own
device, in the shape `i8042-fault` already has: a kernel feature that makes one
read fail, plus an in-guest sequence that fills the cache, forces an eviction
into the failing block, and reads it twice. Two device reads is the assertion —
one means the slot stayed bound and the second reader got the previous tenant.
Roughly 80 lines of kernel and 40 of harness; not built.

### USB mass storage: what is not implemented

The driver serves one logical unit per device and speaks the SCSI commands a
disk needs. Deliberately absent, each with its reason:

- **Multiple LUNs.** `GET MAX LUN` is not issued and `bCBWLUN` is always 0. A
  card reader with four slots presents four LUNs and this would see the first.
- **UAS** (USB Attached SCSI, protocol 0x62). A modern enclosure advertises
  both; the driver takes the BOT interface, which every such device still
  offers. UAS is a different transport with its own streams support in the
  endpoint context.
- **CBI/CB** (subclass 0x00–0x05, protocol 0x00/0x01). Floppy-era transports.
- **READ(16)/WRITE(16).** The driver refuses a device whose last LBA does not
  fit 32 bits rather than serving its first 2 TiB. `READ CAPACITY(16)` *is*
  implemented, because it is how such a device reports the size that gets it
  refused — and `Profile::UsbDiskHuge` is the only place either runs.
- **Removable media.** No `PREVENT ALLOW MEDIUM REMOVAL`, no unit-attention
  handling beyond the `REQUEST SENSE` that clears it during bring-up. A card
  swapped under a running system is not noticed.
- **MODE SENSE.** Write-protect is discovered by a WRITE failing, not in
  advance.
- **Concurrency.** One command at a time per controller, under the xHCI lock,
  with preemption disabled for its duration. Fine at boot; a filesystem doing
  real I/O over USB will want the queue depth the transfer rings already allow.

### A control transfer that stalls during enumeration leaves EP0 halted for good

Filed, not fixed, and visible on any boot of `Profile::MetalFullSpeed`:
QEMU's `usb-wacom-tablet` stalls SET_PROTOCOL, and the driver logs
`xHCI: SET_PROTOCOL on port 6: status stage completion code 6 (Stall Error)` and
carries on.
A stall halts EP0, and nothing clears it — there is no `restart_endpoint` for a
control endpoint. Harmless today because enumeration issues no further control
transfer to that device and the interrupt endpoint is configured afterwards
regardless, so the tablet binds and delivers. It stops being harmless the moment
anything wants to talk to a bound HID over EP0, which is what the mass-storage
path already does on its recovery path.

The same hole one level up: if `reset_recovery`'s Bulk-Only Reset request itself
stalls, EP0 is halted and only the *bulk* endpoints are restarted.

### `bot`'s length assertion names `MSC_DATA_LEN` and binds a different buffer

Filed, not fixed. `bot` asserts `data_len as usize <= MSC_DATA_LEN` (32 KiB),
and four of its five call sites point at `MSC_SCRATCH`, whose length is 64.
The assertion permits a 32,768-byte transfer into a 64-byte buffer. Today's
largest is 36 (INQUIRY) so there is no live bug; the next command added is
where it becomes one, and the assertion is what the person adding it will read
to decide the buffer is big enough. Same shape as `IpcPayload`: a bound in the
right place with the wrong operand. The fix is to give `bot` the *region*
rather than a physical address it cannot reason about. `usb-storage.md` F6.

### `READY_BUDGET_NS` bounds the retries, not the boot time it claims to

Filed, not fixed. The comment says "Boot time is what is being protected, and
boot time is what this measures". It measures when to stop *starting*
attempts and bounds nothing about the one already running: a device that NAKs
indefinitely costs one CBW timeout (2 s), then Bulk-Only Reset (2 s), then two
CLEAR_FEATURE(HALT)s (2 s each) — about 10 s of the boot for one device
against a 500 ms budget, times however many such devices are on the bus.
`Profile::MetalUsb` puts six on one controller. The honest statement is that
`READY_BUDGET_NS` bounds the retries and `USB_TIMEOUT_NS` times what each
costs, and the *product* is the boot-time figure. `usb-storage.md` F11.

### CLOSED — three of a second USB audit's findings, and what fixing them turned up

An independent audit of the mass-storage stack — a second pass over the same
files after `specs/type-safety-audit/usb-storage.md`'s findings landed —
produced eleven more. The three serious ones are **fixed**, each with a gate
that was red against the code before it.

- **F-A. The controller's byte count was discarded, so a lying device served
  stale DMA as file data.** `bulk` returns the residue the xHC reports — the
  bytes it did not move into the buffer — and `bot`'s data phase threw it away
  with `_`, so `delivered` came from the CSW's `dCSWDataResidue` alone. Two
  accounts of one number exist and the driver kept the untrusted one. The
  `MSC_DATA` window is never cleared between transfers (`MSC_SCRATCH` is, which
  is why the capacity read was protected and the bulk data path was not), so a
  device that under-delivers a READ(10) and reports a residue of zero handed the
  caller the previous transfer's bytes for the part that never arrived — a
  different LBA's data under this LBA's number, with no error anywhere.
  `Bot::Done` now carries `delivered`, the smaller of the two accounts. Gate:
  `usb_short_read`, whose actuator is the `usb-short-read` kernel feature.

- **F-B. A plug on an earlier controller renumbered every disk, and mounts hold
  their number for life.** The machine-wide index was `storage.len()`
  accumulated across controllers, and that vector grows on every bind including
  hot-plug. Indices were stable against *unplug* by design and never against a
  plug on an earlier controller. The T14 has two xHCIs — Thunderbolt at 00:0d.0
  first, PCH at 00:14.0 second — and boots off a stick in a PCH port, so
  plugging any USB storage into the USB-C side made the new drive disk 0 and the
  boot stick disk 1: every later `/log` append into the middle of that drive,
  every `/boot` read serving its bytes as the ESP's. The number now comes from
  `DISKS_BOUND`, a machine-wide counter, and lives beside the disk rather than
  being derived from a position. `disk_base` is gone with it — the same defect's
  second face, fixed at boot, so two disks logged under one number the moment
  anything hot-plugged. Gate: `usb_disk_index_stable`.

- **F-C. A refused disk's pool block was never returned, even after it was
  unplugged.** `bind` claims a block before Configure Endpoint and keeps it
  through every refusal, which is right while the device is on the bus;
  `teardown_port` released one only for entries in the disk list, and a refused
  disk is not in it. `MSC_BLOCKS` is 2, so one unsupported stick plugged and
  pulled beside the boot stick left the pool with nothing for any later disk for
  the life of the boot — on a machine whose only diagnostic channel is the
  `/log` it can then not mount. `msc_taken` and `storage` are now one
  `[MscBlock; MSC_BLOCKS]` keyed by the port that claimed the block. Gate: the
  second half of `usb_refused_disk_first`.

  The audit noted that `msc_taken`'s doc comment asserted the opposite in prose
  — "a refused disk never gives its block back. An unplugged one does, and the
  difference is not a policy" — which was false for a disk that was both.

Two one-line findings from the same audit, also fixed:

- **F-G.** `framed_phase` accepted only `(CC_SUCCESS, 0)`, so a status phase
  completing with Short Packet and no residue — every byte of the CSW arrived —
  was an error. It is `Some((CC_SUCCESS | CC_SHORT_PACKET, 0)) => Ok(())`.
- **F-H.** `request_sense` accepted a residue of 5, which leaves ASCQ at byte 13
  unread and reading as the zero `Scsi::unimplemented` tests for. Stated now as
  `delivered >= 14`, which is the fact rather than its complement.

**Found while fixing, not by the audit: the teardown released the pool block
before it disabled the slot.** `teardown_port`'s own doc comment states the
order — input source, then slot, then pool block — and says why the last step is
only safe there: while the slot lives, its endpoint contexts still name that
memory. The code did the block first. Nothing claims a block between those two
statements today, so it was latent; it is now in the stated order.

**Not recorded: F-D, F-F, F-I and F-K.** They are in task #145's description
with their file and line and were not carried into the agent's prompt; writing
them from memory would be inventing them. Whoever holds that task should paste
them in. F-E is the EP0 recovery path, which has an entry above; F-J is the
file-cache error channel, which is its own task.

### `BlockError` is a marker type because `SyscallError` has no I/O variant

`BlockDevice`'s error is a unit struct in `kernel/src/block.rs`. None of
`SyscallError`'s nine variants means "the device did not do it", and adding one
is an ABI change that wanted discussion rather than a unilateral edit.

**The call sites it was waiting for exist now.** `BlockIO` is fallible, so a
device failure reaches userland rather than being swallowed at the filesystem
boundary — and there are three places it arrives as the wrong word:
`fd::try_write` on a request whose first page fails, `BcacheFsAdapter::list`,
and every `vfs::FileSystem` method whose return type is an `Option` or a `bool`.
The first two say `SyscallError::Unknown`; the third says "no such file".
`SyscallError::Io` is the thing to add, and it needs the owner.

## 10. The stick's two partitions as filesystems, and the log on one of them

`/boot` and `/log` are both `kernel/src/fat32_adapter.rs` over `toyos-fat32`,
mounted from `gpt::boot_volume()` and `gpt::log_volume()`;
`kernel/src/log_file.rs` writes one file per boot to `/log`, named for the wall
clock. Gated by `esp_filesystem`, `kernel_log_file`, `log_backing_read_error`,
`log_partition_automount`, `log_partition_identity` and `wall_clock_file`
(`tests/common/volumes.rs`, `tests/common/wallclock.rs`), and `toybox_cp_volume`
(`tests/common/toybox.rs`).

### `Sink::append`'s error return is correct and no longer reachable from a boot

Task #140 replaced the single appended `kernel.log` with one file per boot. The
sink therefore always *creates*, and within a boot its own pages stay resident —
every append sets the CLOCK reference bit on the page it is appending to, so it
is the last page eviction would take. The partial write into a page that has to
come off the stick, which is what `file_cache::write_page` re-reads for, is
consequently something the sink no longer does.

What that costs is one link of a chain, not the hazard: `write_page`'s
merge-into-a-failed-read is unchanged and is reached by anything appending to a
file that already has bytes on the volume, which is what
`log_backing_read_error` now stages — the host writes the file, a process
appends inside it, and the refusal has to reach that process. The claim that
went untested with the trigger is the propagation through `Sink::append` →
`Sink::flush` → `poll` and the sink disabling itself. That code is still there
and still correct by inspection; nothing exercises it.

Reaching it again needs the sink to append to a file with bytes already on the
device, which no shipped path now produces. Worth revisiting if `log_file` ever
grows a resume-an-existing-file case; not worth contriving one for.

### CLOSED for `/boot`, open for `/log` — userland writing the stick it booted from

`/boot` had no permission model of any kind. Proved rather than reasoned: a
guest test binary running `fs::write("/boot/toyos/kernel.elf", "TEETH")`
truncated the kernel image to five bytes.

A mount now states whether userland may change it (`vfs::UserAccess`, given at
every `mount` call and defaulted nowhere) and `/boot` says no. The six syscalls
that can change a volume — `open` for write/create/truncate/append, `unlink`,
`rename`, `mkdir`, `rmdir`, `symlink` — ask `Vfs::user_may_modify` and answer
`PermissionDenied`; reads are untouched. `esp_files` runs the original attack
and each of the other five, and the host half of `esp_filesystem` reads the
build artifacts back out of the image the device received and requires them
byte-identical.

**Three residuals, in order of how much they cost.**

1. **`/log` is `ReadWrite`, `kernel.log` included.** A process can truncate the
   kernel's own log, or fill the volume. It is deliberate — `toybox`'s file
   tools write there, and the worst loss is the diagnostic rather than a
   machine that will not boot — but "the kernel's own volume is not userland's
   to write" is only half done while it holds.
2. **It is a mount-level policy, not a capability.** There is no way to say
   "this process may write `/boot`", so a future installer has nothing to ask
   for. `specs/capability-handles-spec.md` is where that lives.
3. **The FAT32 write path's guest-side coverage moved to `/log`** — same
   adapter, same driver, so nothing is lost, but `esp_files` no longer proves
   anything about writes reaching *the ESP*, because it may not make any.

### The mount is not certain, and one failure is unexplained

Across the boots recorded while this was built, `esp: no boot volume` (as the
line then read) appeared
on a handful. Two instances are explained and closed: `gpt: device 16 has no
partition table we can use: EntryArrayCrc { … }`, which is a *read* off the
stick coming back wrong, from the window where `BlockDevice::read_blocks`
returned `()` and `DeviceSectors::read_lba` served the previous block's bytes
under the new block's tag — closed at `3c5a7b8` and `kernel/src/gpt.rs`'s cache
now drops the tag with the read. One instance after that fix is unexplained,
because the failing run was not captured with serial output. `esp_lines` now
includes `gpt:` and `usb-storage:` lines in the failure message, so the next
one will say which it is.

### `/boot` exists only on a machine that boots from USB

`fat32_adapter::mount` resolves the `DeviceId` in `gpt::Volume` through
`usb_storage::open`, and there is no second arm. A machine that boots from an
internal disk has its NVMe taken by `page_cache::init` at storage time, and
there is no second handle to it — so `gpt::boot_volume()` would answer and the
mount would still refuse. Closing it means either a shared block-device handle
or moving the page cache off sole ownership; neither is a two-line change, and
the machine this project targets boots from a stick.

### A `FatBacking` outlives the file it names, exactly as `/home`'s does

`FileSystem::delete` on this mount drops the *write* handle unconditionally, so
a `write_page` through an fd held across an unlink returns `"file not open"`
rather than putting one process's bytes into another's clusters — which is more
than the bcachefs adapters do. The read side is unchanged and shares §1's live
cross-process leak: an `Arc<FatBacking>` already handed to the file cache still
names byte ranges the allocator is free to reissue.

### The bound is one generation, and after a rotation the newest bytes are in the older-looking file

`kernel.log` rotates to `kernel.log.1` at 4 MiB and the previous `.1` is
deleted. A rotation can be the last thing a boot does, which leaves
`kernel.log` empty and the tail in `kernel.log.1` — so anything reading the log
has to read both. `kernel_log_file` asserts the shutdown's last line is in one of
them rather than in `kernel.log`, for that reason.

### The panic path does not write the log, deliberately

Not a gap to close later: `log_file`'s module documentation states the argument.
A panic-time flush needs the sink lock, the VFS lock, the file cache lock, the
heap, the log volume's device lock and the xHCI lock, and a panicking thread may hold any
of them — so it would deadlock in precisely the cases the log exists for. The
second half of this argument used to be that a torn FAT write leaves the volume
holding `BOOTx64.EFI` and `kernel.elf` unbootable; with the log on its own
partition that is gone, and the worst a half-finished write costs is the
diagnostic itself. The lock argument stands alone. The panic path keeps the
on-screen console, which takes no lock at all. What the file has after a panic
is everything up to the last idle pass.

### The kernel log's flush is unbounded, uninterruptible, and in front of the scheduler pass

Not closed, and it is the residual under gate A's red run. `idle_loop` is
`drain_serial(); log_file::poll(); pass()`, so a wake that arrives while a CPU is
inside the flush waits for the whole filesystem write plus a device cache sync
before any pass can dispatch it. `wait_transfer` spins, and `Lock` holds off
preemption, so nothing shortens it.

Measured in-guest: **7.2–26.0 ms per flush** before the resident-block change,
against a DMA pipeline depth of 23.219 ms — a single flush could empty the
entire audio pipeline. After it, 2.0–9.7 ms, which is what let gate A pass, and
still a third of a pipeline at the tail.

Two premises in `log_file`'s own documentation do not hold, and both are worth
carrying:

* *"It costs nothing when nothing is logged."* True of `log!`, and the ring is
  shared with **userland console output** (`SerialWriter::console` →
  `log_ring::write_chunk_blocking`), so every `println!` any process makes is a
  device write from the idle loop. soundd's own 2-second stats line is one.
* *"A busy machine reaches the idle loop rarely, so each flush carries more."*
  A busy machine has idle *CPUs*: at `--smp 8` seven of them are in this loop,
  and at `--smp 1` the machine is idle between audio periods. The one gate A
  config that did **not** regress was `audio_tone_load.smp1` — the only one
  whose single CPU is never idle. That fingerprint is what identified the
  module.

What it would take to remove rather than shrink: the flush has to become
resumable, or move off the idle path into something the scheduler can preempt.
Both are design decisions and neither is a bounds check.

### What the adapter does *not* re-check about the partition table

`toyos-gpt`'s own residuals (a `last_usable_lba` that may cover the backup GPT,
and two entries in one table sharing a unique GUID resolving first-wins) are in
`specs/type-safety-audit/storage-stack.md` and are the parser's to fix. The
adapter deliberately does not duplicate them: it cannot know whether an extent
is *right*, only whether it is being respected, and two copies of a rule that
can disagree is worse than one. What it does enforce is that no I/O leaves the
extent it was given — and, tighter, that none leaves the FAT volume inside it,
since `Fat32::probe` reads the sector count before anything can write.

### OPEN, unassigned — `cache_eviction` wedges or faults on an *idle* CPU after the test has exited

Seen three times in one session on `main` at `b0e69c5`. The in-guest test
always succeeds: `cache eviction ok: 1168 page reads verified`, exit code 0, at
3.6-5.0 s. What fails is what happens afterwards.

- Full-suite run: `KERNEL PANIC: read unmapped address at 0x58` at 3.615 s on
  cpu1, `#PF SKIP: cr2=0x58 rip=0xffff80007d48f396 err=0x0 (no tid, not user)`,
  12 ms after `exit: test_rs_cache_eviction pid=2 code=0`.
- Two of three isolated re-runs: the harness times out after 180 s, with
  `!!! DOUBLE PANIC !!!` on cpu1 `tid=0` at 66.1 s — 61 s after the exit.
- One of three: clean pass.

**Not the page cache's fallible-read change**, which landed just before it.
Every error path that change added logs a line, and `grep` over the failing
run's serial finds zero of `could not be cached`, `serving zeros`,
`write-back .* failed`, `no slot could be freed`. The cache did the 1168 reads
and reported them correct.

The shape points at the idle path rather than at the workload: no current
thread, after the process is gone, on the CPU that is not running the test.
`4a1f898` and `a10c459` put a log sink on the idle loop that writes a file to
the boot stick, which is new code running in exactly that state and reaching a
block device through a filesystem. That is a lead, not a diagnosis — nobody has
symbolized `rip` against the boot's `Kernel memory located at` line yet, and
that is the first thing to do.

**Measured since, and one contributing cause closed at `5bb1193`.** The
per-CPU idle stack was 16 KiB of ordinary heap with **no guard page**, so an
overflow there did not fault — it rewrote whatever the allocator put
underneath, and a `BTreeMap` node with an out-of-range index (seen: `slice::
get_unchecked` in `CpuSched::drain`) or a write to `0x4` is what that looks
like from the scheduler's parked map. **It has one now**: `alloc_idle_stack`
takes a 4 KiB page out of the direct map below every idle stack
(`paging::guard_4k`, which splits the 2 MiB leaf that covers it), so an
overflow faults where it happens and is reported instead of being found later
somewhere else. `idle_stack_guard` is the gate — the guard page is the one
page in the kernel deliberately absent from the direct map, and absence is
invisible to every log line, so `test-idle-guard` supplies the one read that
touches it. Note what it does *not* change: a fault on a kernel address is
fatal by policy either way, so the machine still halts; what is new is that it
halts with a report naming the address. Instrumented at the block layer, with the
USB command path still below the probe, the sink's path used **11,505 bytes of
the 16,384**. Three 4 KiB page buffers accounted for most of it —
`Vfs::flush_file`'s, and `file_cache`'s two miss buffers, which were
`[0u8; PAGE_SIZE]` handed to `Box::new`. Moving all three to the heap took the
high water to **6,209**.

What that does *not* establish: that the overflow happened. 11,505 plus the
xHCI/MSC chain is close to 16,384 but nothing was caught crossing it, and the
A/B is only three runs each way — three clean with `log_file::poll` removed from
the idle loop, three not clean with it. If it recurs at `5bb1193` or later, the
stack is no longer the first suspect and the `rip` symbolization is.
