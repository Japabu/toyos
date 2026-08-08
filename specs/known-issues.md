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

### Four exception gates are installed with no test behind them, and each for a different reason

Closed by `wt/toyos-idt`: every vector Intel names for 64-bit mode now has a
gate, and `fault_gates` is the guest test. What that test cannot reach is worth
keeping, because it is the list a later change makes reachable.

Measured on the QEMU profile the suite boots, off `info registers` on a live
guest whose GDT limit says the kernel had already loaded its own:
`CR0=0x80010033`, `CR4=0x00310668`.

- **#NM (7)** — CR0.TS and CR0.EM are both clear, so nothing can raise it. Only
  a lazy-FPU scheme would, and there is none.
- **#AC (17)** — CR0.AM is clear, so RFLAGS.AC buys a Ring 3 process nothing.
  The `ac` arm sets AC, reads it back as 1, and the misaligned load still does
  not fault.
- **#MC (18)** — CR4.MCE is **set**, by firmware, and the kernel never clears
  it, so a machine check does arrive on vector 18 rather than shutting the
  processor down. Nothing in the harness can stage one. Handled as an abort:
  `machine_check_handler` halts from either ring rather than killing a process
  over a machine that has stopped being trustworthy.
- **#XM (19)** — CR4.OSXMMEXCPT is set, so the architecture delivers this one;
  TCG does not. With the SSE invalid-operation exception unmasked, `0.0/0.0`
  leaves `MXCSR=0x00001f01` — IE raised, IM clear — and takes no trap. Real
  hardware would fault, so this arm is untested rather than unreachable.

Two more are kernel-only by construction and stay untested: **#TS (10)** needs
a task switch or an `iretq` to a bad TSS, and **#NP (11)** needs a descriptor
with `P = 0` inside the GDT limit, which this seven-entry GDT does not have.

And **#SS (12) is not reachable under TCG at all**: QEMU raises `EXCP0D_GPF`
for every non-canonical access and models #SS for none, so both `ss` arms — one
through RBP, one through RSP itself — come back as `SIGBUS … general protection
fault`. On metal the SDM gives #SS for an SS-relative non-canonical address, so
that gate is exercised by the same arms on hardware and by neither here. It is
the vector the AMD `SYSRET` residue in §3 would arrive on.

### A Ring 3 process that sets RFLAGS.TF floods the log forever and is never killed

`popfq` at CPL 3 sets the trap flag, and every instruction after it raises #DB.
`debug_handler` prints a 25-line `HARDWARE WATCHPOINT HIT` report and
**returns** — it clears DR6 and DR7 on the way out, neither of which is TF, and
`iretq` restores the saved RFLAGS with TF still set. So the next instruction
traps again, forever.

Measured with a throwaway `tf` arm on `fault_gate_child` (three instructions:
`pushfq`, `or qword ptr [rsp], 0x100`, `popfq`): **56 and 58 reports** in the
two five-second boots the harness allows, every one `mode=user`, the child
still running when the guest was killed, and the test red on a timeout. The
rate is low only because each report is 25 lines of serial.

Pre-existing and not introduced by the gates work — vector 1 was one of the six
that always had a gate. Two things are wrong and they are separable: the
handler is a debugging facility that a userland process can summon at will, and
it resumes a fault it has no way to stop. #DB from Ring 3 with no debugger
attached has one correct answer, and it is the one every other fault gets.

### No context switch saves x87 state

Verified by grep across `kernel/src`: there is no `fxsave`, `fnsave` or
`fsave` anywhere. XMM0–15 and MXCSR *are* saved and restored on all three
Ring 3 entry paths — `arch/syscall.rs`, `idt/timer.rs`, `idt/device_irq.rs` —
and the x87 register file, control word, status word and tag word are on none
of them. So one process's x87 state, including a pending unmasked exception, is
visible to the next.

Latent rather than active: Rust on `x86_64-unknown-toyos` does all float work
in SSE and never touches x87, and the kernel is `x86_64-unknown-none`, which is
soft-float.

Recorded here because it is the only hypothesis on offer for a **#MF that goes
missing under load**, and that one is not settled. `fault_gates`' `mf` arm sets
IM, computes 0/0, and expects the `fwait` two bytes later to trap. It killed
the child 6 of 6 alone and survived once in a 12-wide suite — and the status
word it printed on the run that survived was **`0xb881`**: IE set, ES set,
TOP=7, which is our own sequence's state, on the same `fnstsw` two instructions
past the `fwait` that should have raised on exactly that ES. So the state was
not lost; the trap was. Not explained by the missing save on its own, and not
explained at all. The arm no longer asserts its exit code.

### sshd's keys are as protected as any other file, which is not at all

`/home/root/.ssh/host_ed25519` is the machine's SSH private key and
`/home/root/.ssh/authorized_keys` is the list of who may log in. There is no
user model and no file permissions, so **any process on the machine can read
the first and rewrite the second** — the second being the one that matters:
appending a line to it is a remote login, and nothing stops a process doing it.

This is not an sshd defect and cannot be fixed inside sshd. It is the absence
of the thing `specs/capability-handles-spec.md` is about — an owner for a
kernel object, and a process that holds fewer rights than the machine.
Deliberately not worked around here: a daemon-private hiding place would be
obfuscation, and inventing a user model to serve one daemon is the wrong shape
for the decision. Until there is one, **sshd's trust boundary is the machine,
not the account** — anyone who can run code on it can already be anyone.

The daemon does what it can from where it stands: it is not in any boot config,
it offers public keys only, an `authorized_keys` entry carrying options
authorizes nothing (the options are the restrictions, and honouring the key
without them grants more than the file says), and a host key that exists but
does not parse is refused rather than replaced, because minting over it would
change the identity every client has pinned.

### Nothing connects to sshd, so its accept path is read-verified

`tests/sshdcase` boots sshd with a NIC and certifies the half that needs a
machine: that it mints an identity under `/home`, that it names the file it
authenticates against, and that with no usable key it exits instead of holding
port 22. The decision itself — this key yes, that key no, an options line
never — is host-tested in `userland/sshd`'s own `#[cfg(test)]` module against
real Ed25519 keys and `ssh-key`'s parser.

What neither reaches is a client. No test completes an SSH handshake, so the
wiring between russh's auth callbacks and that decision — `auth_publickey`,
`auth_publickey_offered`, and the `MethodSet` that stops password auth being
offered at all — is certified by reading. Closing it needs an SSH client on the
host talking to the guest through `hostfwd`, which is
`specs/daemon-testability.md` §6's step 1 and belongs with gate N.

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

  **The setup is gone too** (tasks #61/#170): a listener is refcounted by the
  descriptors naming it (`listener::ListenerRef`), so the `close(fd)` in that
  three-call attack unregisters nothing and the real compositor's `listen` is
  *refused*. What is left is a squat, which is the "no namespace" bullet in the
  next entry and a different defect. `abuse_listener_hijack.rs` now asserts that
  refusal; the `ListenerId` half is the second line and is no longer reachable
  from userland at all, because nothing can produce a descriptor whose listener
  is gone.
- **`SharedToken`** — a bare `u32` with no RAII and no ownership, still open
  (§7).
- **A device claim** — same shape, closed by the same tasks. `dup` cloned
  `Descriptor::Keyboard`/`Framebuffer`/… as a plain value while `close` released
  the class unconditionally, so `open_device(d); dup(fd); close(fd)` freed the
  device for anyone to take *and* left the caller a working descriptor: two
  processes composing to one scanout, or one reading another's keystrokes.
  `device::Claim` is now a non-`Clone` token whose `Drop` releases the class, so
  `Descriptor` cannot be `Clone` either and `Descriptor::duplicate` cannot
  answer `Some` for those five variants — `dup`, `dup2` and a spawn `fd_map` all
  say `PermissionDenied`. `device_claim_lifetime.rs` is the exploit test.

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
`DeviceType::Audio` that includes the RT band, which audio spec §9.4 wants to be
a privilege. "Gated" here does not mean "privileged".

What is no longer *also* true is that the claim leaks: a claim admits exactly one
descriptor now (`device::Claim`, `Descriptor::duplicate`), so racing the daemon
is the whole remaining attack rather than the cheap half of it.

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

Left alone again by the `toyos-elf` extraction, for the same reason and one
more: the whole change is inside `sys_dlopen`'s arm, where the handle is minted
and where the process's module list lives, and that arm is `arch/syscall.rs`'s.
The extraction touched two lines of it. Whoever takes this owns the *handle*,
not the loader.

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

### CLOSED — two crafted-ELF kernel panics the first hardening wave did not reach

Both are arithmetic, both are reachable from a file any process can write, and
both are panics rather than refusals because the kernel builds with
`overflow-checks` on — `kernel/Cargo.toml`'s `[profile.dev]` says so
deliberately, and `--build-only` builds `debug`. Measured on the host by
compiling each expression exactly as `elf.rs` had it, with
`-C overflow-checks=on`:

- **`e_phoff` = `u64::MAX`.** `elf.rs:475` computed
  `ph_offset + ph_entry_size * e_phnum` in `usize` and then sliced with it.
  `e_phoff=0xffffffffffffffff e_phnum=1: PANIC`, and
  `e_phoff=0xffffffffffffff9b e_phnum=4: PANIC`. With overflow checks *off* it
  wraps to an inverted range and `&data[a..b]` panics instead, so there was no
  configuration in which it was an error return.
- **`.gnu.hash` with `bloom_shift >= 32`.** `elf.rs:1033`'s
  `(h >> bloom_shift) % 64` shifts a `u32`. Shifts 5 and 31 are fine; 32, 33,
  63, 64 and `u32::MAX` all panic. Reachable by `dlopen`ing a crafted `.so` and
  then binding anything against it — `gnu_dlsym` is called on *other* loaded
  libraries, so the crafted one need only be in the process's list.

Closed in `toyos-elf`: the header's table extent is `checked_add` on `usize`
against the buffer, and `GnuHash::parse` refuses a `bloom_shift` of 64 or more
and does the shift in `u64` (which is what glibc's own lookup does on x86-64).
Host cases `a_program_header_offset_that_overflows_is_refused` and
`a_hash_header_that_cannot_be_used_is_refused`; the second was watched go red
with the bound removed.

### CLOSED — the loader leaked a `.strtab` per spawn, for a map nothing read

`build_symtab_map` ended in `Vec::leak(strtab_data)` so it could hand back
`&'static str` keys: up to `MAX_HEAP_ALLOC` of kernel heap, never freed, once
per spawn of a binary that exports nothing through `.dynsym` and loads at least
one library. The map it produced was then dropped without a reader —
`loader.rs:691-735` built it, logged its size and ended. The *live* path
(`:795-850`) rebuilt a `.dynsym`-only map with no `.symtab` fallback at all, so
the fallback that block's own comment describes never once ran.

Closed by the `toyos-elf` extraction: both maps are `loader/symbols.rs`, they
borrow the tables the caller read and die with the spawn, and the fallback runs
where the comment always said it did.

### The bootloader sizes every allocation from a file the ESP handed it

`bootloader/src/main.rs:61-62` reads the UEFI-reported `file_size()` and
allocates that much for the kernel and the initrd, with no bound.
`:103,112` takes `max(p_vaddr + p_memsz)` over the kernel ELF's segments with no
overflow check and allocates it, then `:122` copies `p_filesz` bytes into that
`p_memsz`-sized buffer without checking `filesz <= memsz` — the kernel's own
`toyos_elf::Layout::parse` enforces that, the bootloader does not — and it is
now a crate the bootloader could depend on, which is the cheapest form this fix
will ever take.

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

### `PciDevice::read_bar_64` cannot see an I/O BAR, and reads the next register as one

`kernel/src/drivers/pci.rs:118`. It takes bits 2:1 of the low dword as the
Memory Space BAR's Type field without first checking bit 0, which is the bit
that says whether the register describes memory at all.

On an I/O BAR bit 0 is set, bit 1 is reserved, and **bits 31:2 are the port
number** — so bits 2:1 read as `(0, address bit 2)`. A port whose bit 2 is set
therefore decodes as Type `0b10`, the 64-bit encoding, and the function reads
the *next* BAR register as the upper half of an address. The other half of the
same defect is quieter: with bit 2 clear it returns the port number with the low
nibble masked off, as a physical address. There is no encoding of an I/O BAR
this function refuses.

Nothing reaches it today. `read_bar_64(0)` on an NVMe or xHCI controller and the
BARs a virtio capability names are memory BARs on every part that exists;
`enable_msix` is the one caller whose index comes out of a device-supplied
field, and that path now refuses the reserved indicators and an unassigned BAR
(`toyos-pci::msix`) — but not an I/O one, because the type is not in the
register it decodes.

The fix is a typed BAR decode beside those, and it changes the signature: a
caller that wants memory has to be handed an `Option<u64>` and say what it does
without one. Four call sites in three drivers, one of them in `xhci/`.

### `KernelSlice` is the last `&[u8]` over memory userland can write

`user_ptr` hands out no reference to user memory any more (M1b of
`specs/memory-boundary-spec.md`), but that is a statement about addresses
*userland chose*. `mm::region::KernelSlice::as_slice` (`mm/region.rs:52-54`) is
the other direction: a kernel allocation the loader later maps into a process,
so the borrow is created before the aliasing exists. `elf.rs:973` builds a
`&str` out of one — a `dynstr` symbol name read during relocation.

Whether it is reachable depends on #159, not on this. `vma_map` passes
`writable = true` unconditionally, so a `LibMemory::Shared` image one process
already has mapped is writable by that process while `dlopen` relocates the
same image for another. Fixing the protection (M2, `Protection` as a type)
closes the aliasing and makes the borrow honest; converting the borrow first
would describe a hazard that M2 removes. Recorded so the two are known to be
one question rather than two.

### CRITICAL — the NIC's virtqueue is inside the page netd is granted writable, so netd can aim the device at any physical address

`virtio_net::init` registers **the whole 2 MiB `DmaPool` page** as one shared
region (`kernel/src/drivers/virtio_net.rs:208-209`) and `device::try_claim`
grants it to whoever claims `DeviceType::Nic` (`kernel/src/device.rs:136-141`).
Every shared mapping is writable — `SharedRegion::map_into` passes
`writable = true` with no alternative (`kernel/src/shared_memory.rs:64`,
`mm/paging.rs:534-556`) — and netd maps the full 2 MiB
(`userland/netd/src/main.rs:47`).

**What is in that page besides buffers.** The layout
(`virtio_net.rs:29-37`) puts the RX descriptor table at offset 0, the RX avail
ring at `0x1000`, the RX used ring at `0x2000` and the entire TX virtqueue —
descriptors, avail and used — at `0x3000`; only from `0x4000` on is it the
buffers netd is meant to have. Derived from the constants in that file: 4096 +
516 + 2052 bytes for the RX rings, 256 + 36 + 132 for the TX ones, **7088 bytes
of virtqueue control structure, the last of it at `0x31a7`**, all of it inside
the window. `NicInfo` (`toyos-abi/src/net.rs`) tells netd only
`rx_buf_offset`/`tx_buf_offset`; the *mapping* is not bounded by what it was
told.

**Three primitives follow, in severity order.**

1. **An arbitrary physical write.** Each of the 256 RX descriptors carries a
   `u64` physical address the device will DMA the next frame into. All 256 are
   posted at init (`virtio_net.rs:234-237`) and stay posted until frames arrive,
   so at rest netd holds 2048 bytes of live DMA targets that the device has not
   read yet. Rewriting one aims the NIC at any physical address in the machine —
   kernel text, page tables, another process. `refill_rx` rewrites the
   descriptor from `rx_phys[buf_idx]` on the *next* refill (`:68-78`), which
   closes nothing: the device's write happens first, and the frame contents are
   the attacker's too. Nothing else stands in the way — `kernel/src/iommu/mod.rs:11`
   says of itself that "this module *refuses nothing*", every function is in one
   identity-mapped domain, and stages I0–I2 are all that is built.
2. **Kernel memory onto the wire.** The TX descriptor at `0x3000` is written by
   `submit()` and read by the device. Same window, opposite direction: a
   rewritten `addr`/`len` reads arbitrary physical memory out through the NIC.
   Narrower than (1) as a race — under TCG QEMU services the notify inline — but
   it is a race only because of how the host schedules, not because of anything
   the kernel does.
3. **Forged completions.** The RX used ring at `0x2000` is what
   `Virtqueue::poll_used` reads back. See the entry below for what the kernel
   then does with an `id` and a `len` it did not check; that entry's "a buggy or
   malicious *device*" becomes "netd" here.

**The fix shape is already in this tree, one file over.** `virtio_sound::init`
allocates *two* pools (`virtio_sound.rs:374-375`): `dma_kernel` holds the
descriptor tables and the TX used ring and is never registered; `dma_shared`
holds the avail rings and the buffers and is the only token soundd is given
(`:429-433`). A forged avail entry can then only name a chain the kernel itself
built, and the comment at `virtio_sound.rs:403-405` states the rule for the used
ring in as many words. virtio-gpu registers only its framebuffer and cursor
pages and keeps its `DMA` pool private (`virtio_gpu.rs:464-479`, `:678`).
virtio-net is the one device that hands its virtqueue out, and it predates both.

**Standing.** No test stages a netd that writes outside its buffers, and none
can while the mapping is one 2 MiB grant. `specs/type-safety-audit/kernel-drivers.md`
F1 audits the *reading* half (the entry below) and does not reach this: it
argues from a misbehaving device throughout, which is a hardware-failure
argument, where this is a process-isolation one. Splitting the pool the way
virtio-sound does is the whole fix and needs no ABI change — `NicInfo` already
addresses everything by offset.

### `poll_used` returns a device-chosen descriptor id and length, both unchecked

`Virtqueue::poll_used` (`kernel/src/drivers/virtio.rs:387-399`) reads `id` and
`len` out of the used ring — memory the *device* writes, and for virtio-net
memory *netd* writes too (entry above) — and returns `DescSlot(id as u16)` and
`len` with no comparison against `self.size` or against any buffer length.
`UsedRingConsumer::poll` (`:191-205`) does the same for `id`. `DescSlot` is this
codebase's own proof token, deliberately non-`Copy` and non-`Clone`
(`:169-172`), and it proves the descriptor is *free*; it says nothing about the
number being in range, and `id()` is public.

**Three consequences in the code today.**

- **An out-of-bounds read, in `unsafe`, from an unchecked length.**
  `virtio_console::try_read_byte_locked` (`virtio_console.rs:132-148`) stores the
  returned `len` and walks `*c.rx_ptrs[p.buf_idx].add(p.pos as usize)` until
  `pos >= len`. `RX_BUF_SIZE` is 256 (`:29`) and the eight RX buffers sit 256
  bytes apart inside one page (`:186-191`), so a `len` above 256 walks the next
  buffer, then `OFF_RXVQ`'s virtqueue rings, then past the pool — all inside the
  direct map, so it faults nowhere and delivers kernel memory to the console as
  typed input.
- **A kernel panic from an index.** `desc_to_rx` is `[u8; 16]`
  (`virtio_console.rs:56`) and `desc_to_buf` is `[u16; 256]`
  (`virtio_net.rs:59`), and `slot.id()` ranges over the whole `u16`. Rust bounds-
  checks both, so the failure mode is a panic rather than a read — and on the
  virtio-net side the value comes out of a page netd writes, which makes it a
  userland-triggered kernel panic, the thing CLAUDE.md's corollary forbids by
  name.
- **A frame length nothing bounded.** `poll_rx` returns
  `written_len as usize - NET_HDR_SIZE` (`virtio_net.rs:90-101`) and
  `SYS_NIC_RX_POLL` packs it as `((buf_idx as u64) << 16) | (frame_len as u64)`
  (`kernel/src/arch/syscall.rs:482`) with no mask, so a length above 65535
  corrupts the buffer-index field of the word netd unpacks, and netd's own
  `rx_buf(idx)` walks off its mapping.

**One consumer of the four does check.** `virtio_sound::drain_tx`
(`virtio_sound.rs:110-118`) rejects a head that is not a chain's and counts it as
`stray` rather than trusting it; its comment at `:100-102` states the rule —
"a head that is not a chain's is untrusted input, not a device fault". virtio-net,
virtio-console and virtio-gpu do not, and virtio-gpu's `submit` masks a bogus id
with `% size` (`virtio.rs:346`) so it silently aliases another submission's
descriptors instead of failing.

**Standing.** Audited and explicitly never filed:
`specs/type-safety-audit/kernel-drivers.md` F1 has the full analysis and the
proposed fix at the primitive — `submit` records each chain's byte total, and
`poll_used` answers `None` on an id past `size` or a length past the chain — and
its closing line reads "**Standing.** … Not filed." This is that filing. Two of
its citations have since drifted: `virtio.rs:161-164` is `:169-172` today, and
the two `virtio_sound.rs` `assert!`s it names as the counter-example are gone,
replaced by the `stray` counter above. No test covers any of it.

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

### CLOSED — under KVM the kernel takes a #GP returning to userland, on AMD only

**Closed 2026-08-07 by `wt/toyos-sysret`: the RPL in `STAR[63:48]` was the
CPU's to supply and only one vendor supplies it.** `SYSRET` builds both user
selectors out of that field — SS from it plus 8, CS from it plus 16. Intel's
SDM forces RPL 3 into both (`SS.Selector := (IA32_STAR[63:48]+8) OR 3`); AMD's
APM forces it into CS alone and takes SS's straight from the field. The kernel
put a bare `0x10` there, so on an AMD host every user thread ran with
`SS = 0x18` instead of `0x1b`.

Nothing notices while it runs — 64-bit mode does not check a data segment. The
*return* is what dies: an interrupt taken from such a thread pushes `SS = 0x18`,
and the handler's `iretq` is a return to an outer privilege level, where
`SS.RPL` must equal `CS.RPL`. 0 is not 3, so `#GP(SS selector)` — the error code
is literally the selector.

The frame the faulting `iretq` was consuming, printed from the #GP's own
handler (run `31214761003`, EPYC 7763 and EPYC 9V74, same line on both):

```
!!! SEG frame@0xffff800000dcffd8 rip=0x1000008fbaa cs=0x23 rflags=0x246
        rsp=0xfffffff6d0 ss=0x18 | here cs=0x8 ss=0x10 err=0x18
```

**Vendor, measured.** Run `31212538770`: eight `ubuntu-24.04` runners, eight
boots each, all eight drawing an AMD EPYC (7763 or 9V74) — **64 of 64 red**,
every one at `timer::timer_entry+0x174`, which `llvm-objdump` puts on the Ring 3
path's `iretq`. Two Intel runners drew in other runs of the same probe (Xeon
6973P-C, Xeon Platinum 8573C) and passed. So the original A/B — TCG green, KVM
red on run `31205890425` — was the accelerator *and* the vendor at once, and
only the vendor mattered: QEMU's `helper_sysret` implements Intel's wording, so
TCG behaves like an Intel CPU whatever the host is.

**With the fix, run `31219204355`**, same probe, six runners: five drew an AMD
EPYC (7763 and 9V74) and one an Intel Xeon, **39 of 40 AMD boots green** where
the same probe was 0 of 64. The fortieth is `process_stats: timed out after 5s`,
twice in a row, on a guest whose log ends at `===READY===` with soundd up and no
fault anywhere in it — a harness liveness budget on a shared runner, which is
never a verdict (CLAUDE.md). Recorded rather than chased: the `kvm` job in
`ci.yml` runs this test once, so it is that job's flake rate and it is not zero.

**Two things this cost that are worth keeping.** The first reading of the report
took `Syscall: num=86 user_rip=…` as evidence the fault was in the syscall stub;
that line is the *last syscall this thread made* and `crash_report_exception`
prints it for every kernel-mode fault, so it says nothing about where the fault
is. And the report printed sixteen GPRs and not one segment register, which is
what made a #GP whose error code is a selector unreadable — `cs`, `ss` and
`rflags` are in it now.

**Still open on this path, and unmeasured:** AMD's `SYSRET` does not reload SS's
*cached descriptor* either (Linux calls this `X86_BUG_SYSRET_SS_ATTRS`), so a
`sysretq` taken while `SS` is NULL leaves userland with an SS that looks like
`0x1b` and raises `#SS` when used. The kernel can reach that — an IDT entry from
Ring 3 nulls SS, `timer_handler` calls `do_preempt`, and the incoming thread may
be one parked in a syscall that returns by `sysretq` — and it has no `#SS`
handler, so the escalation would be `#DF`. Not observed: the full suite is green
under KVM on AMD with the RPL fixed (see the KVM job in `ci.yml`). Linux's
workaround is one `mov ss, __KERNEL_DS` in its context switch; ours would go in
`KernelHw::switch`, which is the single site.

### CLOSED — two CPUs shooting down at once wait for each other and both panic

**Closed 2026-08-07 by `wt/toyos-tlbfix`: `arch::tlb::shootdown` waited without
ever answering, and every path that reaches it has `IF` clear.**
`arch::syscall`'s `MSR_FMASK` masks `IF` on the `SYSCALL` gate and nothing sets
it again before `sysretq`, so a CPU inside the wait could not take vector 0xFE
and had no other way to acknowledge. M3 closed that class for `Lock::lock`,
whose spin calls `arch::tlb::poll` on every turn, and enumerated the `IF=0`
spins it thought were left; the wait itself was not on the list and is one.
`Shootdown::wait_turn` is one turn of the wait and it answers before it asks —
asking first lets a CPU leave on the answer it just received without ever
publishing the generation its sibling is waiting on. Gate:
`an_initiator_answers_while_it_waits` in `kernel-loom`, which is this entry's
schedule written out, red on the old shape.

Everything below is the evidence as it was recorded, and it is what made the
diagnosis: two backtraces, read together, are the whole defect.

Observed 2026-08-07 on `wt/toyos-h3`, whose only kernel delta from `main` is the
audio one; the shootdown code is `c4173f0` and `318ec10`, landed the same
afternoon. Reproduced twice inside gate A's thorough tier and once more in a
twelve-run hunt of `cargo test -- audio_tone` — roughly one boot in five of that
family. **The machine goes down**, so it is a double kernel panic, not a test
failure.

```
[kernel 8.597 cpu4 tid=1] !!! PANIC !!!: panicked at src/arch/tlb.rs:133:42:
  tlb: cpu 0 has not flushed for generation Generation(2) in 5000000000ns — it is not taking interrupts
    kernel::arch::tlb::shootdown+0x1dc
    drop_glue::<Vec<kernel::mm::unmapped::Unmapped<kernel::process::PageAlloc>>>
    kernel::process::thread_exit+0x61b
  Running: pid=2 test_rs_audio_tone, syscall num=5 (SYS_THREAD_EXIT)

[kernel 8.599 cpu0 tid=0] !!! PANIC !!!: panicked at src/arch/tlb.rs:133:42:
  tlb: cpu 4 has not flushed for generation Generation(3) in 5000000000ns — it is not taking interrupts
    kernel::arch::tlb::shootdown+0x1dc
    kernel::arch::syscall::sys_release_shared+0x9a
  Running: pid=0 soundd, syscall num=39 (SYS_RELEASE_SHARED)
```

**Read the two together: cpu4 is waiting for cpu0 and cpu0 is waiting for cpu4.**
Each is inside `shootdown`'s `wait_for`, each spinning on the other's flush
acknowledgement, and each times out at the 5 s deadline. Neither is serving the
other's IPI while it waits. The generations differ by one, which is the two
initiators having taken `SHOOTDOWN`'s sequence in opposite order.

The workload is ordinary and is not audio's: a thread exiting and freeing its
unmapped pages on one CPU, while another process releases a shared-memory region
on another. soundd's client stream ring is the region here only because that is
what the audio tests run.

**Attribution, and its limit.** No frame in either backtrace is in code the audio
branch touched, and gate A's A arm — 60 audio boots of `80fe031`, which predates
both shootdown commits — never produced it. That is strong and it is not a
bisect; whoever takes this should confirm against `5ef66f0` (main immediately
before the audio landing, with the shootdown work and without it).

**What it costs right now**: gate A's thorough tier cannot complete a run on any
tree carrying it, because a panicked guest is scored as an instrument failure and
the tier stops. That blocked H3's own A/B (§4).

**Independently reproduced from a third worktree**, same day, across four
landing gates, on a branch whose every changed kernel line sits behind
`#[cfg(feature = "heartbeat")]` and so is compiled into none of the tests that
failed. Same victims, same two lines in every capture. The reading worth
carrying forward: **the name on the red is the workload that was running and
never the cause**, so grep a red run's log for `tlb:` before believing the test
name — including when the harness re-runs one alone, reds again and reports
"the defect is real", which `metal_sim_window_caps` did.

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

### A CPU that stops scheduling keeps publishing load 0, and `placement()` therefore prefers it

The T14's Ctrl+Alt+D dump (`boot9-dump.log`, 35.181 s) reports `5/8 cpu(s)
answered`. cpu1, cpu4 and cpu7 each failed to reach a scheduler pass inside the
dump's 250 ms budget. Their last lines in that whole 63 s boot are the idle
loop's own:

| CPU | last line, and it is the last thing it ever said |
|---|---|
| cpu1 | 1.152 s — `sched: cpu=1 ready=0 parked=0 current=None` |
| cpu7 | 11.231 s — `sched: cpu=7 ready=0 parked=1 current=None` |
| cpu4 | 26.110 s — `sched: cpu=4 ready=0 parked=1 current=None` |

**Whatever stops them is a separate defect. This entry is about what the
scheduler then does about it, which is the opposite of the right thing.**
`CpuHandle::publish_load` is written by a CPU at the end of each of its own
passes, so a CPU that takes no more passes publishes its last value forever —
and its last value is the one it wrote on the way into idle, which is **zero**.
`driver::placement` picks `min_by_key(load)`. A dead CPU is therefore not merely
a CPU that never runs anything again; it is the CPU the scheduler *prefers* for
every subsequent spawn.

That is the difference between losing a core and the machine getting
progressively worse, which is what the owner reports and what the log shows:
three cores shed over 26 s, and by the end the dump finds `pid=10 terminal`,
`pid=6 tid=2 doom` and `pid=12 shell` all `ready and has never run`, plus
`soundd` ready with 2 ms of CPU. doom's black window is its sound-init thread
placed on a shed core; the missing shell prompt is `shell pid=12` on another.

A load figure is a claim about the present, and a stopped CPU's is a lie the
scheduler believes. The fix is independent of the root cause and worth having
either way: placement must not be able to choose a CPU that has not completed a
pass in some number of intervals. The counter to compare against already exists
per CPU (`publish_load`'s call site is the end of every pass, so a monotonic
pass count published beside the load costs one relaxed store), and the same
staleness test would let `idle_sibling` and `post_steal_probe` stop aiming at
dead cores too.

Not fixed here: the instrument that names *why* a CPU stops (NMI probe,
`arch/idt/nmi.rs`) landed first, because a placement filter over an unknown
cause would hide the cause.

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
one defect. Ctrl+Alt+D is now machine-wide and process-named (§5), and on the
owner's laptop one press named three CPUs not reaching a scheduler pass and
three threads ready-and-never-run.

### OPEN (#156) — seven T14 boots in seven minutes, and the signature everyone has read as a wedge is what a healthy idle machine writes

**The evidence is committed**: `specs/metal-logs/2026-08-07-freeze/`, seven
consecutive boots of one image off the owner's stick, 22:26–22:33 on
2026-08-07, five of them frozen with Ctrl+Alt+D producing nothing on every one.
Its `README.md` has the table. This entry is what the seven establish, what they
eliminate, and what they cannot reach.

**Nothing in a frozen boot's log distinguishes it from a healthy one.** Diffed
with timestamps, addresses, CPU ids and TSC jitter normalised, a frozen boot and
the healthy one are identical up to the moment the frozen one stops — the only
differences are SMP join ordering and where one `shm: mapped WriteCombining`
line falls. Four of the five end at the same line, `spawn: /bin/filepicker
pid=3`, between 0.945 s and 1.462 s.

**That ending is not evidence of a wedge, and reading it as one is what has cost
this investigation its last three rounds.** It is what a healthy, fully
quiescent T14 writes, and three separate facts force it:

1. the log ring's only drains are the idle loop and the timer tick;
2. a CPU whose pass finds no work and no deadline stops its LAPIC timer and
   halts (`TimerPlan::Stop`), so on a machine with nothing to run there is no
   idle loop and no tick either;
3. the pre-halt check in `sched::driver::execute` refuses to sleep while
   `log_ring::file_has_pending()`, so the last CPU to go down flushes everything
   first. **The file is therefore complete as of the moment the machine went
   quiet, and silent about everything after it.**

In the healthy boot `223244` the very next line after `spawn: /bin/filepicker`
is the owner's first keystroke, 445 ms later. In `223152` it is 1.958 s later.
The log has no other reason to say anything.

**One of the five freezes is closed outright, and it is the control the other
three needed.** In `222741` the firmware left the controller at `cfg=0x30` —
translate **off**, where every other boot in the set reads `0x77` — the set
query answered `0xEE` as it always does on this EC, and `init` took its
fail-closed branch:

```
i8042: ok selftest=0x55 cfg=0x30->0x60 port1=ok port2=ok
i8042: kbd DISABLED - the set query answered 0xee and firmware's cfg 0x30 has
       translate off, so nothing says what the wire carries
```

It returns before the aux port, so that boot had **no keyboard and no
TrackPoint**, by the driver's own correct decision. Ctrl+Alt+D could not have
worked on it whatever the scheduler was doing. And its file is the same shape as
the other three — same ending, and earlier only because the refusal returns
before the keyboard and aux stages: `Boot: peripherals ready (448ms)` against
`(841ms)` on every other boot in the set. A boot with provably no input path is
indistinguishable from the ones being investigated.

**`223152` is the fifth, and it is the sharpest.** Input worked exactly once —
`the pin asserts — 1 interrupts, 1 bytes, 1 keys` at 3.397 s — the filepicker
acted on it, `/bin/terminal` and `/bin/shell` spawned at 3.494 and 3.539, and
then nothing ever again. That is the same shape as "the T14 lost every
integrated input at 6.6 s" in §8, on a different boot and a different image.

**Eliminated, by reading and not by a run.** `drain_serial`'s `BackendGuard::lock`
was the named suspect and it cannot be this. `SERIAL_SINK` is false for the
whole boot on a machine with no 16550 and no virtio-console, so `append` never
advances `LogRing::len`, `OWED` stays 0, `drain_into` returns 0 on the first
call and `drain_serial` leaves its loop having held the guard across one memcpy
of nothing. §8 reached the same conclusion by the same route in a different
week; it is restated here because the hypothesis keeps coming back.

**What is left, and none of it is decidable from these logs.** Three hypotheses
that all produce exactly the file above:

- **A.** The machine is alive and quiescent, and the i8042 has stopped
  delivering. Three sub-causes, opposite in where the fault lies: the controller
  holding a byte no ISR will read (delivery is edge-triggered, so it never
  asserts again), a redirection entry that got masked or re-pointed, or an EC
  that simply stopped sending.
- **B.** CPU 0 is deaf — spinning with `IF` clear, or wedged — and the machine is
  otherwise alive. **On this kernel that is indistinguishable from a total
  freeze from outside**, because `pci::MSG_ADDR` targets APIC 0 for every
  device's MSI and `init` routes GSI 1 and GSI 12 to APIC 0 as well: every
  interrupt source the machine has lands on one CPU.
- **C.** The machine stopped.

#### What to do at the machine, in order

**Zeroth, and it costs a glance: what is on the panel?** Nobody has recorded it,
and the compositor's own source makes it a real split rather than a curiosity.
`Session::new` ends:

```rust
let mut damage = Damage::default();
damage.add(desk.screen);
eprintln!("compositor: ready");
Command::new("/bin/filepicker").spawn().ok();     // <- the last line in four of the five logs
```

Nothing has been painted at that point: the wallpaper was rescaled into RAM, the
whole screen was *staged* as damage, and the first composite happens in the
caller's loop, **strictly after the `spawn:` line the frozen logs end on**. The
panel therefore answers a question the log cannot:

- **wallpaper** (with or without the filepicker's window) → the compositor
  returned from that syscall and completed its first frame. The machine got past
  the last line in its own log, and the three hypotheses below are the live ones.
- **the kernel's boot log, 8x16, still from `boot_checkpoint`** → it did not.
  `screen_claimed_by_userland` fires at the *claim*, several lines earlier, so
  the checkpoint had already stopped repainting and what is on the glass is the
  last thing the kernel put there. That puts the failure between the compositor's
  `spawn()` and its first blit — one syscall wide, on the process that had been
  running fine a millisecond earlier — and it is #142's shape, not a scheduler
  that stopped.

`223152` is the one boot where this is already known: it ends at `spawn:
/bin/shell pid=5`, two composites and one keystroke past that boundary.

**First, and it needs no reflash: plug a USB keyboard into the frozen T14.**
This is the input-independent source the dump has always needed and it already
exists — `keyboard::handle_key` is the single production path for every keyboard
in the machine, the Ctrl+Alt+D hook is inside it, and `xhci::hid` reaches it
through `keyboard::handle_report`. A hotplug raises a Port Status Change Event,
whose MSI wakes a halted CPU, and `poll_if_pending` enumerates from `drain_irqs`.
So:

- `xHCI: port N connected` appears in `/log` and the keyboard types → **A**, and
  the machine was alive the whole time.
- Ctrl+Alt+D on it paints the dump → **A**, with the full scheduler state as a
  bonus.
- Nothing at all → **B or C**, which the build below then separates.

**Then the one reflash: `cargo run -- --build-only --kernel-feature heartbeat`,
and flash `target/bootable.img`.** The *ordinary* image and not `--diag-boot`:
the freeze happens with the desktop up, and the diagnostic image has no
compositor, so it is a different workload and would not be a re-run of these
seven. *The heartbeat was never compiled into the image that produced them* — no
`alive=` line appears in any of the seven — and it is the instrument built for
exactly this question. It brings
`diag-tick`, so no CPU sleeps longer than 100 ms and the idle loop keeps running
on a machine that has gone quiet. Reading it:

| what the log does | which hypothesis |
|---|---|
| `alive=8/8 … ran=0` continues through the freeze | **A** — and the `i8042: line` beside it says which sub-cause |
| `alive=7/8 mask=0xfe` and `heartbeat: cpu0 last reached one N.NNNs ago` | **B**, dated |
| heartbeats stop at T | **C**, dated |

The `i8042: line` beside every heartbeat is new (this task) and is what makes
**A** actionable rather than merely named: `status` with bit 0 set on sample
after sample is the controller holding a byte, bit 16 of an `rte=` is the mask,
and a clean reading with `irqs=` flat puts the fault at the EC and takes this
driver out of it. `kernel_heartbeat` gates it against the vector `init` says it
programmed.

**Two things about that build, said out loud.** It is an *active* instrument:
`diag-tick` holds the machine out of full quiescence, so a freeze that needs
deep idle may not happen at all under it — which is itself a finding, and worth
recording rather than re-running away. And four heartbeats a second each carry a
`sync_mount` of the stick, so it is a diagnostic budget and not a shipping one.

#### What was deliberately not built

**A heartbeat that summons the dump by itself** when a CPU has been missing from
the mask for several periods. It is the obvious next step for **B** — it would
turn `cpu0 last reached one 4.2s ago` into a symbolised `rip` through
`dump::probe_silent`'s NMI, with no keystroke needed — and `dump::request` is
reachable from the idle loop as written (preempt count 0, holds nothing). It was
not built because it needs an actuator of its own to be gated: `dump-deaf-cpu`
stages a 400 ms window and calls `request()` itself, so it can neither reach a
multi-period threshold nor let a test attribute the dump. **The owner reflashes
once**, and an ungated path in that image is worth less than the resolution it
would add. Whoever picks it up should build the actuator first.

### OPEN, UNASSIGNED — the total freeze now reproduces in QEMU, in about seven seconds

**This is the first reproduction of the freeze class.** Everything above was
read off T14 logs because nothing in the suite could stage it; `desktop_window_child`
(landed 2026-08-06 by the compositor track, for a different property) stages it
by accident and reliably.

`cargo test -- desktop_window_child`. `tests/desktopcase`, `Profile::Metal`,
`smp: 8`. Round 0 is clean: `/bin/snake` spawns, GUI+Q closes its window,
`exit: snake pid=7 code=0` and the shell answers `after-snake-0-zqjxk`. The next
round spawns `snake pid=9`, the shell echoes `/home/root> snake`, **and the
guest emits nothing further at all** — not the exit, not the shell, and not the
compositor's stats line, which had been arriving every ~2 s until that instant.
The harness drains every 200 ms for 20 s and appends nothing, which is why the
assertion's message body is empty: the emptiness *is* the evidence.

Ten attempts, ten reds, across four separate invocations; the round it dies in
alternates between 1 and 2, so what varies is the timing and not whether it
happens. The same tree was green once, on the landing gate that put the test in
(verified by ancestry: `ce3e09d` and `7a9e5c1` are both ancestors of that
branch's merge), so the single green is the outlier rather than the reds.

Why it matters beyond one red test: **the owner's unrecoverable freeze on
pulling the USB stick, where Ctrl+Alt+D produces nothing, is the same
signature** — no CPU reaching a scheduler pass. That one is only reachable on
his laptop and only through a photograph. This one is reachable under LLDB
(`gdb-remote 1234`, every CPU's state inspectable, `--debug` parks the guest),
which is a different order of cost. **Two cheap experiments nobody has run yet:**
press Ctrl+Alt+D at the frozen guest over QMP — that answers directly whether a
pass-dispatched dump can fire on a total freeze, which is the open design
question against the NMI proposal — and if it says nothing, attach and read the
eight RIPs.

It also blocks every landing until it is fixed, because it is on main and it is
not flaky.

**RESOLVED as a contradiction, 2026-08-07: the two agents were watching two
different failures, and the silence is no longer reachable through this test.**
`40ee9a6` found that `close_focused_window` waited on `windows=N` — a level
sampled every two seconds — with `serial_until`, which scans the whole capture,
so the previous probe's sample answered the wait instantly and the loop re-sent
GUI+Q at the speed of a QMP round trip. The second one closed the terminal's
window under the one it meant, and the three exits that followed were correct
behaviour. **That signature is three logged exits in 0.25 s; the one recorded
above is total silence with no exits at all**, so the fix did not explain the
silence, it removed whatever was being poked hard enough to produce it.

Ten runs of `cargo test -- desktop_window_child` alone on `8cfb6d8` after the
merge: **ten green, no silent guest**. So the silence is not reachable through
the shipped harness any more, and the LLDB-attachable reproduction the class
still needs is not this one. What was *not* tried, and is the cheap next step
for whoever wants it back: restore the old function's injection rate
deliberately — a burst of GUI+Q at QMP round-trip speed while windows are being
created and destroyed — as a test of its own rather than as a regression.

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

**CLOSED in the tree, and the fix is the SDK's rather than the fork's.**
`Window::poll_event` latches: `Close` is the last element of the stream and is
delivered exactly once, so a caller that drains until `None` gets out. That
makes the fork's loop correct as written, and it fixes every other client that
drains the same way rather than winit alone. `desktop_window_child` and the
fifth case of `compositor_client_death` gate it from the client side. A break
on `Close` in the fork's poll loop is prepared but unpushed (owner task #150);
it is now belt-and-braces rather than the fix.

**The T14 confirms it.** In `boot8-snake.log` the owner closed snake's window
with the X button and snake exited `code=0` — where before the latch it never
exited at all. The `ControlFlow::Wait` and `WaitUntil` arms at `:637` and
`:707` are the ones snake actually runs (`userland/snake/src/main.rs:351`,
`:359`), and they are now tested rather than read: snake leaves on a close both
on his machine and in `desktop_window_child`, three rounds, one of them played.

**What that session leaves open is not snake.** 34 ms after snake exited the
shell exited too, and 13 ms after that the terminal — so the owner got no
prompt back. That is the next entry.

### EXPECTED RED, pending #156 — `desktop_window_child` reproduces the machine freeze, and must stay `Sched::Parallel`

**Read this before touching that test.** It is red on `main` today, on purpose,
and the two obvious ways to make it green both destroy the only QEMU
reproduction anybody has of #156.

**The signature, precisely, because there are two different reds it can give.**
The one that means #156 is a **total freeze of the guest**: a round opens
snake, the shell echoes its prompt, and from that instant the guest emits
*nothing at all* — not `exit: snake`, not a shell line, and **not the
compositor's `compositor: frames=…` stats line, which had been arriving every
~2 s until then**. The harness drains its full ceiling and appends nothing. The
missing periodic line is the discriminator: any other failure of this test
leaves output flowing and fails an assertion with the log still filling. If you
see the test fail *with* serial output continuing, that is a different defect
and this entry does not cover it.

Independently hit 10/10 across four invocations by an agent trying to land
unrelated documentation, in the 12-wide parallel phase, with the harness's
re-run-alone pass reporting GREEN each time.

**It stays `Sched::Parallel`, and `ALONE: GREEN` here is information rather
than a misclassification.** The freeze needs contention to appear, so the
classification that looks wrong is the one that reproduces it. `Sched::Serial`
would make the suite green and take the reproduction with it. (The classifier
is not trustworthy in general — the xHCI work established that `ALONE: red
again` can measure the host rather than the tree — but in this direction, on
this test, green-alone is real.)

**A third manifestation, 2026-08-06**, on the mechanism branch, twice in one
session — once in the 12-wide phase and once in the re-run alone, at 3.5 s into
both boots. The message is `GUI+Q never reached the compositor`, **and it names
the wrong thing**: the close did reach the compositor. The log under it is the
teardown, one probe earlier than the snake rounds — `exit: test_rs_window_child
pid=5 code=0`, then `exit: shell pid=2 code=0`, then `exit: terminal pid=1
code=0`, then `windows=0`. `close_focused_window` waits for `windows=1` and the
desktop went straight to none, so the harness reports the injection it did
deliver as one that never arrived. Serial kept flowing for the whole drain —
compositor stats every ~2 s, kernel stats every 10 s — so by this entry's own
discriminator it is **not** the freeze; it is the shell-exit defect three
paragraphs down, reached at the first windowed child rather than during a snake
round. Whoever fixes that message should make it say what the log says.

**And that third manifestation is now the only one this test produces, which
means the reproduction is masked (2026-08-06, eight boots).** Two full-suite
runs in the 12-wide phase and six alone — three with the winit lock at
`be9ec72c`, three at `faf99eb7` — are red every time and **not once the
freeze**. Every capture has the guest alive for the whole drain: compositor
stats every ~2 s, kernel stats every 10 s, the i8042 counter climbing to 4818
keys as the harness re-injected GUI+Q, and all eight vCPUs `HLT=1 RFL=0x246`,
which on a settled desktop is an idle machine and not #156. What all eight show
is the teardown above, now at the **second** probe — the owner's case, a live
client whose window is taken away — with the client leaving `code=0` before the
shell does. One of the eight got through to snake round 0 and produced the same
shape with `exit: snake pid=7 code=0 cpu=1224ms` in its place, so where the test
stops varies with load and what happens does not.

Two consequences for whoever picks this up. **`ALONE: GREEN` no longer holds** —
6 of 6 alone are red — so the paragraph above it is a record of what the test
used to do, not a prediction. And **the freeze's venue is unreachable**: it was
seen in a snake round, and the desktop is torn down one or two probes before
that. Fixing the shell-exit defect is now on #156's critical path rather than
beside it. The teardown is not a regression from the deadline fix (`add6aeb`,
18:05): the paragraph above it was written at 17:32 describing the same three
exits, and is not a descendant of it.

**CLOSED, and it was the harness closing the desktop twice (2026-08-06).** The
teardown in the three paragraphs above is not a guest defect at all.
`close_focused_window` looped on `log[new..]` but waited with `serial_until`,
which scans the whole capture, so the *previous* probe's `windows=1` answered
the wait instantly and it re-sent GUI+Q at the speed of a QMP round trip. The
second one closed the window under the one it meant. The compositor now says so
itself and the two closes are one line each:

```
compositor: window closed pid=5 by GUI+Q, 1 left
exit: test_rs_window_child pid=5 code=0
compositor: window closed pid=1 by GUI+Q, 0 left     <- the terminal
exit: shell pid=2 code=60
exit: terminal pid=1 code=0
```

Everything after the second close is correct: the terminal breaks on
`Event::Close`, drops `shell_stdin`, the shell's stdin reaches genuine EOF
(`60` is `UnexpectedEof`, encoded into the exit status because a diagnostic
through that pipe is lost with it), the shell leaves and the terminal reaps it.
So the shell-exit reading below — "the failure is the read after the prompt" —
was right about the mechanism and wrong about the cause: nothing was wrong with
the read, its writer had been told to go.

The fix is `note_closed` in the compositor plus a harness that waits on that
event instead of sampling `windows=N` every two seconds
(`scheduler-migration-log.md`). **`desktop_window_child` now passes end to end,
alone and in the 12-wide phase** — both windowed-child probes and all three
snake rounds, snake leaving `code=0` each time.

**What this does *not* settle is the freeze**, and the entry stays. It stays
`Sched::Parallel` for the same reason as before, `EXPECTED_FAILURES` keeps its
declaration to its review date, and a green run still proves nothing — the
signature at the top of this entry is a guest that goes *silent*, and none of
the eleven boots in this session produced one. What has changed is that the
test can now reach the snake rounds where the freeze was seen, which it could
not before. Judge the next occurrence by the signature, never by a run.

**Landing while it is red** needs nothing special: `desktop_window_child` is
declared in `EXPECTED_FAILURES` (`tests/toyos.rs`) and `cargo run -- --land`
runs the ordinary gate. The declaration reports it by name on every run, is red
if the test *passes* where the entry says a pass is proof, and is red on
`2026-09-06` regardless — this entry is intermittent, so its own expiry is a
date rather than a green run. The `--skip` flag that used to be the answer is
deleted: an exclusion nobody reviews cannot expire, and this one has to.

**CLOSED — a stale `--skip` command line was worse than a refused one, measured
2026-08-06.** The flag was gone but the words still parsed, and `--land --gate
cargo test -- --skip desktop_window_child` — the form CLAUDE.md and every
handover carried until that week — reached the harness as a *filter*.
It ran exactly one test, `desktop_window_child`, declared it expected, and
landed on that. `--land` does print `the gate was NOT the default cargo test`,
which is the only thing that saved it; the run itself looks like a pass.

The asymmetry that entry named — a stale feature name refused against
`kernel/Cargo.toml` before any lock, a stale gate flag not refused at all — is
closed. `toyos_build::testargs::parse` holds one table of the flags the suite
has, refuses anything else by name and by consequence, and returns the filter
out of the same pass, so no word can be a flag's value to one reader and the
filter to another. It is the first thing `main` does, before the sysroot lock
and before anything is compiled. A flag added to the harness and not to that
table is refused the first time it is typed, which is the direction of drift
that says so rather than the one that narrows a gate.

**What the declaration will and will not absorb.** Its `says` list covers the
six of this test's messages whose failure is *the desktop ceasing to answer
after a window closed*. The other five red the run — the client binary missing,
the desktop never coming up, a window never being created, and the client
leaving on its own deadline. That pins which assertion failed and not why, so
the log-tail discriminator above is still a human's to apply; the run prints the
pointer to this section beside every `XFAIL` line for exactly that reason.

**One thing #156's capture leaned on is closed, and it is not this.** The
deadline was stored twice — `ParkedEntry.deadline` and `DeadlineHeap` — and
`fire_deadlines`' lost claim discarded one copy, so a CPU could halt with
`TimerPlan::Stop` while its report said `1 pending, 0 OVERDUE`. That is why the
dump taken off the frozen guest could not be read, and it is fixed
(`scheduler-migration-log.md`, 2026-08-06): a deadline lives in one place and
invariant T reads what arms the timer. **This entry stays open.** Nothing
established that the divergence is what froze the guest — the claim's
`Msg::Wake` follows it within two instructions and `SleepArm::confirm` refuses
to halt on a non-empty mailbox — and a green run of this test after that change
is the race landing the other way rather than evidence. Judge it by the
signature above, never by one run.

**What the test was built to chase is still open underneath it**, and is not
the same thing: on `boot8-snake.log` the owner closed snake's window and got
`exit: snake pid=10 code=0` at 121.659, `exit: shell pid=5 code=0` at 121.693,
`exit: terminal pid=4 code=0` at 121.706. A shell that has reaped its
foreground child prints a prompt; it does not exit. The harness has now shown
the same three-process teardown after a window close — child `code=0`, then the
shell, then the terminal — with a bare `window::Window` client and no winit
anywhere, so it is not snake's and not the fork's.

**Which of the two readings the evidence supports.** In the harness run the
shell's prompt *is* on the serial log, between the child's exit and its own,
mirrored by a terminal still inside its loop. So the shell reached its prompt
and then went: the failure is the read *after* the prompt, not the prompt. That
is "the shell exits instead of prompting" rather than "the chain is torn down
child-first" — and it points at the shell's stdin, whose only writer is a
terminal that was demonstrably still running. On the T14 the same window shows
no prompt at all, which is the weaker evidence of the two and may simply be a
line that never got flushed.

**A confound that is not this.** Three of the owner's eight CPUs stop taking
scheduler passes during a session and threads placed on them never run (§3,
#142/#156). That produces processes which *hang*; the three here *exited*,
promptly, with `code=0`. This entry must not be closed by it — though the
freeze the test now reproduces is very likely that defect, which is the whole
reason the test is worth keeping red.

### OPEN — `/bin/terminal` races the compositor at boot and exits, and the ready marker hides it

`init = ["/bin/compositor", "/bin/terminal"]` (`tests/desktopcase/system.toml`,
and every other desktop config). The kernel spawns both back to back and
`/bin/terminal` calls `Window::create_with_title` at once; if the compositor
has not reached its `listen` yet, `services::connect` refuses, the terminal
prints `terminal: no compositor is running` and exits 1, and the shell it has
already spawned exits behind it. The desktop then comes up with **no window at
all**.

Nothing notices, because `BootOptions::ready_marker` is `compositor: ready` and
the compositor is fine — it prints that line a few tens of milliseconds after
the terminal has gone. Every later assertion is then made against a desktop
with no terminal in it, and the message is whatever that test says about
typing: `nothing typed at the terminal window reached a shell`.

Twice in one session on a busy host, in different tests — `desktop_locale_detect`
in a 12-wide phase, and `desktop_window_child` in a landing gate whose suite
took 372 s against a quiet run's 135 s. The margin is small: terminal gone at
0.633 s, `compositor: ready` at ~0.7 s. A strong candidate for the
`desktop_locale_detect` half of §6's `Sched::Parallel` red list.

Not fixed here, because *where* the wait belongs is a design question:
`window::Window::create` retrying would make every client wait for a
compositor that may legitimately be absent (`/bin/console` boots with none);
the terminal retrying puts the policy in one client; and sequencing `init` on a
service registration is kernel policy. What is not in doubt is that a client
which starts before its service is listening must not read that as "there is
no desktop".

**Measured 2026-08-08, and it is the dominant blocker of §6's `Sched::Parallel`
red list rather than a candidate for one.** Eight full suites in one session,
two concurrent twelve-wide runs at a time on one host (`--host-slots 0`), four
on `main` and four on the branch that made `shell_echoes` say what it had
found:

| arm | suites | suites with the race in a boot log | red suites |
|---|---|---|---|
| `main` | 4 | 3 | 3 |
| branch | 4 | 1 | 1 |

**Every red suite in the session contained this race and every green one did
not.** The two other reds in the session were `audio_tone_load (smp=8)` and
`xhci_hid_break`, and each landed in a suite that already had one. All three on
`main` reported it as `nothing typed at the terminal window reached a shell` —
once as `desktop_typing_damage`, twice as `desktop_audio_client` — and each was
`ALONE: GREEN`, which is how a defect that reproduces in roughly half of these
suites has been read as host noise. `shell_echoes` now ends that wait on
`exit: terminal ` as well as on `terminal: ready` and names the race, so the red
arrives in about a second instead of holding a lane: 305 s in the run that
produced this table, with `desktop_window_child` beside it at 285 s.

### OPEN — the desktop chain reads every stdio error as end-of-input, and says nothing

Found while tracing #156's teardown, where both of these had to be worked
around before the cause could be read at all.

`userland/shell/src/main.rs`'s `read_byte` is `read_exact(&mut buf).ok()?`, so
a device error, a revoked fd and a genuine EOF are one value: `None`. `readline`
turns that into `break`, `main` returns, and the shell exits **0**. A shell that
died because its terminal vanished and a shell whose stdin failed are
indistinguishable from outside, and both look like a clean exit.
`userland/terminal/src/main.rs` has the same shape twice —
`shell_stdout.read(&mut buf).unwrap_or(0)` for stdout and stderr, where `0` is
its own signal to leave.

Neither says anything on the way out, and the channel that would carry it is
the one that failed: the terminal breaks on stdout EOF **before** it drains
stderr, so the shell's last stderr line is dropped with it. What established
the cause was encoding `io::ErrorKind` into the shell's exit status, because
the kernel's `exit: name pid=N code=C` line is the one record neither end can
swallow.

No reproduction of a non-EOF error here; the one traced was a real
`UnexpectedEof`. The defect is that there could be, and nobody would know.
A fix is a message naming the fd and the kind, and a stderr drain before the
terminal leaves.

### OPEN — the compositor holds a window *index* across passes, and every removal invalidates it

Found while reading the close path, not reproduced. `Interaction`
(`userland/compositor/src/main.rs:825`) carries `window_idx: usize` in its
`DragPending`, `Dragging` and `Resizing` variants, it survives between event
loop passes, and the drag and resize arms index `windows[window_idx]` with no
revalidation (`:1901`, `:1912`). Every path that shortens or reorders `windows`
invalidates it: `remove` for the close button, GUI+Q and `MSG_DESTROY_WINDOW`,
`retain` for the dead-client sweep, and `bring_to_front`'s remove-and-insert.

Two consequences and a client can drive both, which is what makes it the same
class as the grant that killed the desktop: a window removed *below* the
dragged one moves the wrong window, and a window removed *at or above* the last
index makes `windows[window_idx]` an out-of-bounds panic. A client that exits
while the user is dragging is the whole of the reproduction.

The fix is the one the same file already uses one line away —
`last_title_click_fd: Option<Fd>` identifies a window by its `Fd` rather than
its position — and it wants a gate that drops a window mid-drag.

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

### CLOSED — check-and-act across a released global lock, and the three residuals it leaves

`pipe::open_reader`, `sys_socket_create` and six `file_cache::ref_count` call
sites each read an answer out from under a global lock, released it, and acted.
The pipe one was the one with teeth: `exists(id)` then `add_reader(id)` panicked
on `.expect("add_reader: pipe not found")` when the pipe's last other end closed
in between, *inside* `with_pipes_mut` — and a recovered syscall-context panic
leaves `PIPES` held for the rest of the boot, which wedges all IPC. Closed by
making the count and the handle one construction: `PipeReader`/`PipeWriter` live
in a child module of `pipe.rs` whose field is private to it, so the only way to
name a pipe as an owned reference is `acquire(&mut Pipe)`, and only a holder of
`PIPES` can produce a `&mut Pipe`. `exists`, `creator` and `ref_count` are
deleted; `mark_deleted` returns `Residency`; `copy_page_out` returns whether the
page was there instead of zero-filling. Negative controls in the commit message.

Three things the change does not reach:

- **A count can still move without a handle.** `close_read`/`close_write` must
  decrement and decide whether to free the pipe in one acquisition, so they live
  in `pipe.rs` and touch `Pipe::readers` directly. Binding that direction too
  needs the counts in a struct declared inside the handle module with a
  `pub(super)` decrement, which buys a named operation and not an unwritable
  one. The direction that was the defect is closed; this one has two writers and
  both are in that file.

- **`SYS_FTRUNCATE` takes no VFS lock and `SYS_FSYNC` does** (`arch/syscall.rs`,
  `SYS_FTRUNCATE => fd::ftruncate(&mut data.fds, ...)` against
  `SYS_FSYNC => fd::fsync(&mut data.fds, &mut *vfs::lock(), ...)`). That
  asymmetry is what let a truncate remove a page between `clone_dirty` and
  `copy_page_out`. The write of fabricated zeros is closed; the window is not,
  and `flush_file`'s `size()` + `update_metadata` pair sits in it too — a
  truncate landing between them records the older size, corrected by the next
  flush because `ftruncate` sets `modified`.

- **`file_cache::read_page` has two paths that return with `buf` untouched**
  (`file_cache.rs`, both `let Some(file) = cache.files.get_mut(&file_id) else {
  return }`), while `fd::try_read` has already told its caller how many bytes it
  is producing, from a `file_cache::size` read under a different acquisition. It
  is not reachable today — `try_read` runs holding a descriptor for the file, so
  its refcount is at least one and neither `release` nor `mark_deleted` can drop
  it — but the signature makes no such claim, and the honest return is fallible.

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

### CLOSED — `io_uring::remove_fd` cancelled polls and woke nobody

The other half of the entry above, found while working #172 and independent of
it. `remove_fd` posts `-NotFound` for every pending poll on a source that is
going away, so the caller knows the registration is over — and it posted them
into the ring without waking the ring's waiters. **Nothing else could end that
wait**: the poll is gone from `pending_polls`, so its `WatcherGuard` has
already taken the ring off the source's watcher list, and the close path that
runs immediately afterwards (`close_read`/`close_write` → `wake_pipe_readers`)
then finds no watchers and skips `complete_pending_for_event`. A thread in
`io_uring_enter` with `timeout_nanos == u64::MAX` — which is every server in
the tree, and `/bin/console`, `/bin/terminal` and soundd's control thread by
name — never returns.

`fd::close` calls `remove_fd` *before* the descriptor drops, which is what puts
the watcher removal in front of the wake path that would otherwise have covered
it. Two descriptors on one pipe is all it takes, so an ordinary `dup`/`dup2` of
stdio reaches it.

Gate: `io_uring_cancel_wakes` (`tests/toyos-rust-tests/src/bin/`). A thread
registers a `POLL_ADD` on a pipe read fd, proves the registration reached the
kernel with a zero-timeout enter, then parks with `u64::MAX`; the main thread
closes a `dup` of the same fd. Negative control run with the wake removed: the
waiter was still parked 10 s later and the test says so in its own words.

Every other posting path was audited in the same pass and two more were
silent — `post_cqe_locked` (a sibling thread of the submitting process can be
parked on the same ring) and `process_poll_add`'s `MAX_PENDING_POLLS` refusal.
Both wake now. The durable fix remains iouring-blocking-spec's single `post()`,
where posting and waking cannot be separated.

### CLOSED — a dropped completion could only be reported after the call that never returned

`Poller::wait` reads `cq_hdr.dropped` and asserts it is zero, with a comment
saying the assertion should be unreachable. It is read *after* `submit`, and
`submit` is the call that blocks — so in the one failure the counter exists to
explain, a completion dropped while the caller is parked, the assertion is
never reached at all. A tripwire that can only fire after the hang it is about
reports nothing.

`enter` now refuses to park a ring whose `dropped` is nonzero and returns
short, which puts the counter in front of the caller. It is cumulative and
never cleared, so this cannot loop: `Poller::wait` asserts on the very next
look. No test, and it would be dishonest to add one — the state is unreachable
from a conforming caller for exactly the reasons `poller_capacity` gates, and a
kernel feature that manufactured it would be replacing the verdict rather than
the failure.

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

### Five driver waits spin with no deadline, and NVMe reads the timeout the spec gives it and throws it away

`grep -rn "spin_loop()" kernel/src/drivers/` returns 23 sites. Five of them are
unbounded polls of a device register, all on the boot path:

- `nvme.rs:105-119` `wait_completion` — `loop { … core::hint::spin_loop(); }`
  on the completion-queue phase bit, no deadline. **Every** admin and I/O
  command reaches it through `submit_and_wait` (`:121-124`).
- `nvme.rs:434-436` — `while bar.read_u32(REG_CSTS) & 1 != 0` (controller
  disable).
- `nvme.rs:458-460` — `while bar.read_u32(REG_CSTS) & 1 == 0` (controller
  enable).
- `virtio.rs:412-417` `submit_and_wait` — polls `poll_used()` forever. (Its
  *panic-path* instance is filed in §2; this is the ordinary one.)
- `virtio.rs:453-456` — device reset, `while common.read_u32(COMMON_DEVICE_STATUS) != 0`.

**NVMe hands the driver the bound and the driver drops it.** `CAP.TO` (bits
31:24, in 500 ms units) is defined as the worst-case time for exactly the
`CSTS.RDY` transitions at `:434` and `:458`. `nvme.rs:429` reads the whole
`CAP` register and `:430` takes `((cap >> 32) & 0xF)` — the doorbell stride —
out of it; nothing else in the file touches `cap`. So the one number the device
publishes about how long to wait is read into a local and discarded, and a
controller that never sets `RDY` hangs the boot with nothing on the log to say
which one.

**The primitive already exists and is not shared.** The xHCI half of this closed
on its own: those waits moved behind a bounded `settles(ready)` against
`USB_TIMEOUT_NS` = 2 s (`xhci/wait/mod.rs:126-135`, `xhci/mod.rs:319`), and the
legacy handoff is bounded by `HANDOFF_TIMEOUT_NS` = 1 s (`xhci/legacy.rs:55`,
`:177-180`). It is now written three times byte-for-byte against three
constants — `xhci/wait/mod.rs:126`, `hda.rs:758`, `hda_probe.rs:979` — plus two
copies of the spin delay beside it (`hda.rs:769`, `hda_probe.rs:990`), plus
`scheduler.rs:190`'s `wait_until`, plus an IOMMU variant that `assert!`s where
the others return (`iommu/vtd/queue.rs:125-130`, `iommu/vtd/mod.rs:271-276`).

**Standing.** `specs/type-safety-audit/kernel-drivers.md` F10 (`:928`, deadlines
and durations as bare `u64` in two different units, so `wait_writable(500)`
compiles and means "expired at boot") and F11 (`:987`, the
`wait(off, until, pred) -> Result<u32, Timeout>` primitive and its blast radius)
are the design; F11's own closing line is "**Standing.** Not filed." Two
corrections to it: its count of eight unbounded MMIO polls is **five** today,
because the xHCI sites it named are the ones that closed; and `CAP.TO` appears
nowhere in it. **Not** `specs/iouring-blocking-spec.md` — that spec owns the
*park* deadline (§9.1–9.2, `Instant`/`Duration`/`Deadline`, "no `0 = forever`"),
never a driver register poll, and does not mention NVMe at all.

---

## 4. Audio and soundd

### OPEN, UNASSIGNED — `hda_tone` is red on `main` for a reason `#88`'s exemption does not cover

`cargo test -- hda_tone` on `main` at `6d11938`, alone, 2026-08-07 18:5x:

```
FAIL hda_tone: 1 mid-tone silences in the capture: total 1 [1p×1]
  FAIL  hda_tone  (15s)  — listed against #88, and this is not that failure:
        the entry covers ["the captured tone is not one sine"]
```

The `EXPECTED_FAILURES` entry does what it is supposed to: it pins the assertion
rather than the test, so a *second* defect in the same test still reds the run
and says which. What is red is the mid-tone-silence assertion — a gap in the
capture, which is gate A's harm verdict — and not #88's spectral one. So any
landing whose gate is `cargo test` is currently red on `main` for this, and an
agent will read it as theirs.

Found while landing task #98/#12: the same test failed identically inside that
landing's gate, and the A/B against `main` in the same session is what
identified it as `main`'s. Assigning it needs whoever owns H3 —
`5fdfeb7`/`a022811` ("wip: H3, the virtio-sound stub and its userland driver")
landed hours before this measurement.

### CLOSED as to its cause, OPEN as to the landing — the wide phase reds on a five-second TLB stall

**The signature's cause is closed: §3, two CPUs shooting down at once.** It was a
mutual wait and not a bound, so no deadline value was ever going to fix it; the
wait now answers before it asks. What that does *not* do is make `cargo test`
reliably green, and this section stays open for the part it does not reach.

**The wide phase still reds, on a different class, and this signature did not
reproduce here at all.** Measured 2026-08-07 on `wt/toyos-tlbfix`, four full
suites and 96 guest boots of the audio family, **zero `tlb:` lines in any of
them** — including 50 boots on a kernel with the fix reverted, where the H3
agent's twelve-run hunt had found it in roughly one boot in five. So the rate
this defect ran at earlier in the day is not the rate it runs at now, and no
measurement taken here can be read as the fix having lowered it. The fix rests
on §3's two backtraces and on `an_initiator_answers_while_it_waits`, which is
red without it.

| run | wall | verdict |
|---|---|---|
| before the fix | 576.3 s | 4 red: `metal_sim_compositor_stall`, `metal_sim_client_death`, `screen_blocked_dump` (all `ALONE: GREEN`), `audio_tone (smp=8)` |
| after, 1 | 559.3 s | 2 red: `i8042_mouse`, `desktop_audio_client` — 385 s wide against 13 s alone — both `ALONE: GREEN` |
| after, 2 | 182.7 s | **clean, 289/289**, on a host that was briefly quiet |
| after, 3 | 704.7 s | 1 red: `screen_blocked_dump`, `ALONE: red again` |

Every one of those reds is the parallel-red list in §7, not this entry: the two
that name a duration are the contention class, and the one clean run is the one
whose host was idle. **A landing is still a coin toss and the reason is now
squarely §7**, whose own last paragraph says a verdict that flips with the host
is measuring the host. `audio_tone (smp=8)`'s `suspend structure: no
'soundd: suspended' after the last client removal` fired 2 of 12 on the reverted
kernel and 0 of 12 on the fixed one, which at n=12 is not a difference and has
no mechanism behind it — recorded so the next person does not read it as one.

What follows is the evidence as it was recorded on `wt/toyos-boot`, and it is
what pointed at §3.

Measured 2026-08-07 across two `--land` gates on `wt/toyos-boot` (289 tests
each) and five A/B runs against `main` at `6d11938`, all in one session.

Seven distinct tests failed between the two gates —
`null_sink_shipped_client`, `metal_sim_window_caps`,
`metal_sim_ipc_hostile_peer`, `metal_sim_compositor_stall`,
`metal_sim_client_death`, `desktop_window_child`, and an `hda_tone` that is the
entry above. **Every one of their captures carries the same two lines**, with
different generation numbers:

```
tlb: cpu 1 has not flushed for generation Generation(69) in 5000000000ns
     — it is not taking interrupts
tlb: cpu 0 has not flushed for generation Generation(68) in 5000000000ns
     — it is not taking interrupts
```

And every one of them **passes alone, on both trees**:

| test | in the wide phase | alone on the branch | alone on `main` |
|---|---|---|---|
| `null_sink_shipped_client` | FAIL, 10 s | PASS, 4 s | PASS, 5 s |
| `metal_sim_window_caps` | FAIL, 5 s (three times) | PASS, 3 s | PASS, 36 s |

`metal_sim_window_caps` is the clearest: its own work *completes* —
`window caps: oversized refused, 62 windows granted then refused` — and the
process then exits `-1` after two CPUs have each stalled five seconds. 62
windows created and destroyed is 62 rounds of unmapping, which is exactly what
`arch::tlb::shootdown` is on the path of. `null_sink_shipped_client`'s round 1
took 6.14 s for a 3 s tone and its round 2 then panicked in
`toybox/src/tone.rs:85` on `failed to open audio stream: NotFound`.

So the branch is not the variable. The shootdown wait landed on `main` the same
day (`318ec10`, `c4173f0`) and **its own diagnostic is what named the stall**, so
the instrument is already in the tree.

The reading that "the load is the variable" was the wrong half of it, and worth
keeping as the mistake it was: load is what made two unmaps overlap, and the
overlap was fatal by construction. The generations here differ by one — `68` and
`69` on two CPUs of a two-CPU guest — which is the same pair of initiators §3's
backtraces name.

**The load is the variable, not the width.** Four full runs, same session, same
289 tests, one branch:

| width | host | verdict |
|---|---|---|
| 12 (default) | this worktree alone | 2 failed, 287 passed — 526.9 s |
| 12 (default) | this worktree alone | 5 failed, 284 passed — 497.0 s |
| **4** | this worktree alone | **289 passed, 0 failed — 265.2 s** |
| 4 | `toyos-tlbfix` running its own suite | 4 failed, 284 passed — **610.4 s** |

The third row is the one that looks like a fix and is not: the fourth is the
same width against a second worktree's suite, and it is red again with three of
the same victims (`metal_sim_window_caps`, `metal_sim_ipc_hostile_peer`,
`metal_sim_compositor_stall`) and the same `tlb:` lines. What the third row does
show is that **4-wide beat 12-wide on wall clock by a factor of two on a quiet
host** — `specs/test-cost-audit.md` §4.1 constraint 3 arriving from a new
direction, and a measurement worth re-taking on its own terms.

So `--land --gate cargo test --test toyos-build -- --jobs 4` is a way through
when the host is otherwise idle, the landing prints it as an override, and it
fixes nothing.

**Not to be re-run away**: the owner's 2026-08-04 ruling is that a
load-coincident failure is a real defect, and this one reproduced across two
full runs with seven different victims. That ruling is what produced this entry
instead of seven re-runs, and it is what the fix came out of.

### OPEN, UNASSIGNED — gate A's thorough tier is red on `main`, and the recorded dropout sample is what it disagrees with

Measured 2026-08-07 on `main` at `c0365ea`, in one session, three arms:

| tree | dropout runs | measured runs | verdict |
|---|---|---|---|
| `main` | 7 | 28 | `pooled dropout rate: 7 of 40 vs recorded 0 of 120 (Fisher p=4.00e-5)` |
| `wt/toyos-m3` (M3) | 5 | 12 | `5 of 40 … (p=8.02e-4)` |
| M3 with its one new wait deleted | 5 | 40 | `5 of 40 … (p=8.02e-4)` |

The denominators differ because the gate stops as soon as the remaining runs
cannot change the verdict, so each arm ran until it was decided. **All three
fail, and `main` fails hardest.** The gate's own documentation says it cannot
detect a doubling of the dropout rate at any N a human waits for, so it also
cannot separate these three from each other — what it says unambiguously is that
every one of them is far from the recorded `0 of 120`.

Every gap is small and none is a silence anyone would hear as a break: the
largest is 51 periods, most are one or two, and the fast tier — whose verdict is
harm — is **green on all three arms**, 7 of 7 each. So this is a *rate* finding
against a recorded sample, not a report that the machine sounds wrong.

Two readings, and nothing here decides between them:

- the recorded sample in `tests/audio-baseline.toml` was taken with one QEMU at a
  time and no concurrent agents, and this host now runs several worktrees at once
  — `specs/audio-gate-history.md`'s own lesson is that these counters drift
  between batches on one host with no code change;
- or something landed since the sample was recorded and nobody re-ran the
  thorough tier to notice.

The second is testable and the first is not, so **the next step is the thorough
tier on the commit the sample was recorded against**, on this host, in one
session. Do not re-record the baseline first: a sample re-taken now would make
the disagreement disappear without anyone learning which of the two it was, and
the recorded zero is the only reason the question is visible at all.

Host load was 6–20 throughout and is **not** offered as the explanation — the
owner's 2026-08-04 ruling stands, and the three arms ran back to back on the same
host anyway, which is what makes them comparable to each other.

**Consequence for anything landing while this is open:** the thorough tier cannot
serve as a pass/fail gate. M3 used it as an A/B instead, which is what it could
still answer, and landed on the fast tier plus the full suite.

### CLOSED 2026-08-07 — soundd panicked on the T14: `repeated completion for free buffer`

The metal boot at `2026-08-07-222910.log`, `/bin/tone` through the T14's
`00:1f.3` (ALC257, converter 0x02 → pin 0x14, 8 periods of 512 B, stream 7).
soundd died 0.5 s into a 2 s tone on `assert_eq!(free_mask & rec.mask, 0)` with
bit 3; the owner heard the tone as "more like a triangle or sawtooth wave"
before it stopped.

**The mix loop's free list is a queue model, and HDA is a ring.** On
virtio-sound a period soundd has not submitted is a period the device does not
have. HDA's engine owns every period for as long as `RUN` is set: it returns to
buffer *i* exactly `num_buffers` periods after completing it and plays whatever
is there. Three of the free list's rules were the queue model showing:

1. **§5.10's deferral.** `refill_floor_nanos` bounds *unplayed audio* (5 of 8
   periods), not the engine's return, and the selection took the lowest free
   index — so the deferred set pinned at the top three free buffers and the same
   ones were held cycle after cycle while lower ones were refilled. Each played
   the silence `released` left in it, and after one lap the engine completed one
   soundd still held. Deterministic within ~14 wakes of a client stalling. The
   three-in-eight silence is the buzz the owner heard.
2. **§5.8's drain.** With the last client gone the free list filled over one lap
   with no margin: a wake landing more than `num_buffers` periods after the
   first completion sees the lap folded into one mask and trips the same
   assertion.
3. **A completion posted between soundd's read and the stop landing**, which
   arrives on the idle poll against a full free list.

None of the three is reachable on virtio-sound, which is why gate A never saw
any of them, and none of them is reachable in `hda_tone` either: its client
keeps its ring full, so `deferred=0` on every run measured.

Fixed by `Backend::pipeline` naming the difference and the three sites asking
it. A ring gives up a period it cannot fill rather than holding it, fills in the
engine's order off a cursor that follows the engine through a drain and a stop,
and never defers. `unplayed` replaces `free_mask.count_ones() == num_buffers` at
the two drain sites — the same number on a queue and the only correct one on a
ring. `stream::decode` reads the engine's position back off every mask.

**The first version stepped that cursor itself and asserted the next mask
matched it, and the 12-wide phase killed it in one run.** `ISR.mask` accumulates
across interrupts, so what a late driver reads is the OR of several `completed`
calls — and at `max_wake_lat_us` in the tens of milliseconds that is `0xff`, a
whole lap, which places the engine nowhere. A stepped cursor would have to be
right about how many laps went by; a derived one cannot drift. `Completed::Lapped`
is that answer said in words, and it is the drain §5.9 already counts. The wide
phase constructed a state five deliberate `hda_client_stall` runs did not.

Gate: `hda_client_stall`, a client that stops producing for 60 ms (two and a
half laps) and then plays a second stream so a resume is under test too, on both
machines. Both arms are asserted and must differ — the ring must report
underruns and hold nothing, the queue must defer — so neither of the two wrong
fixes goes green. Unfixed on this tree: the ring arm panics and the run times
out at 120 s. Fixed: ring `underruns=128 deferred=0`, queue `deferred=539`.

**Not verified on the T14.** QEMU's `intel-hda` and the laptop's controller are
different devices and only the owner can boot the second. What the next metal
boot should show: `tone` playing to completion with soundd alive, and soundd's
stats line reporting `deferred=0` with `underruns` nonzero if the client ever
stalls. A `the engine completed 0x.., which is no walk of an 8-period ring`
panic would be new, and would mean the T14's `SDnLPIB` moves in a way
`stream::completed` cannot walk — the first evidence that this design's one
position source is the wrong one there (§2.4's `position_fix` paragraph).

**EXPECTED RED, pending #88 — HDA: the captured tone is not one sine.**
`hda_tone` plays the same 3.0 s 440 Hz tone the virtio arm plays, out of an
`intel-hda` controller soundd drives itself, and the capture comes back with
**8 to 16 phase discontinuities** where the virtio arm has none. Declared in
`EXPECTED_FAILURES` against the message "the captured tone is not one sine";
every other assertion that test makes still reds the run.

What is *not* wrong, measured on this host (QEMU 11.0.3, 2026-08-07): the tone
is present at full amplitude, there is **no mid-tone silence at all** (`gaps
none`), and soundd's own counters match the virtio arm's — **1127 periods
submitted, 0 underruns, 0 drains** on both. The guest put the same audio on the
wire; something between soundd's buffer and the wav file did not carry it.

The instrument is new and calibrated: `audio::phase_breaks` tests the recurrence
`x[n+1] = 2·cos(ω)·x[n] − x[n−1]` that a sampled sinusoid obeys exactly, and it
reads **0 on all four recorded virtio configs** (`audio_tone` and
`audio_tone_load`, smp 1 and 8). It is `specs/hda-driver-plan.md` §5.3 item 5's
second guard, built because §2.4's zero-on-complete rule — the thing that keeps
one gap detector valid for both backends — is a design promise with no
measurement behind it (risk 7).

Evidence, and why it does not yet name a cause:

- Six consecutive runs at `timer-period=5000` gave **8 breaks at identical frame
  positions** — 2703-2705, 2821-2823, 2939-2940 — across runs whose audio
  content differed (the dither seed is clock-derived, and the sample values at
  those positions differ run to run). Identical positions with different content
  is a capture dropping samples on a cadence, not a guest playing them wrong.
- Shortening the host's drain interval to `timer-period=1000` moved the
  positions rather than removing them: one run at 0 breaks, the next at 16,
  clustered around frame 95725 instead. **So it is intermittent and the host
  cadence is not the whole story.**
- Within a cluster the breaks sit at multiples of **118 frames**, which is
  neither the device period (128) nor either backend timer's frame count
  (44.1 or 220.5). That number is unexplained and is the sharpest thing here.
- The capture also holds **2.756 s of tone where the virtio arm holds 2.94 s**,
  with no seam accounting for the difference on the runs that show none. Either
  the capture opens late or QEMU's `hda-codec` discards on its own ring
  overrun; both are host-side and neither is established.

Where to start: QEMU's `hw/audio/hda-codec.c` output ring against soundd's
eight-period pipeline. **Do not weaken the check to make it green**; the virtio
zero is what says it has teeth.

**2026-08-07: one guest-side cause found and fixed, and it is not the whole of
it.** soundd filled a completion batch lowest-index-first, so a batch that
wrapped the ring — `{6,7,0,1}` — was filled 0, 1, 6, 7 and played 6, 7, 0, 1.
That is a splice with no silence in it, which is this signature exactly. Six
`hda_tone` runs in one session, instrumented with a counter of fills that were
not in the engine's order, separated cleanly: **one run at `out_of_order=2`,
`max_batch=7`, 9 phase breaks; five at `out_of_order=0`, `max_batch=4`, 0
breaks.** It is fixed (see the entry below) and the fill order is now the
engine's by construction.

**The breaks survive it.** Two runs on the fixed tree gave 8 and 6, with
`deferred=0` and an ordering that can no longer be wrong. So the ordering was a
contributor and not the cause, and the remaining one is still unnamed. What the
new numbers add:

- The break count tracks **soundd's own wake lateness**, which on this host is
  the host descheduling QEMU: 0 breaks at `max_wake_lat_us` 8626–8752 (five
  runs), 8 at 22525, 9 at 16730, 6 at 50837. Nothing in the guest changed
  between them.
- soundd put **1127 periods = 144,256 frames** on the wire and the capture holds
  **131,061** — 9.1% of what was submitted is not in the file, with `gaps none`
  and `underruns 0`. On the run at 6 breaks it is 10.8%. A capture missing a
  tenth of its samples has phase breaks whatever the guest did.

So the next step is the host side. `isr_complete` is a weaker candidate than it
was: `stream::decode` now refuses any mask that is not a walk of the ring, and
soundd fills in the order that walk names rather than in index order.

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

### OPEN — gate A's *thorough* tier reds on an unmodified `main`, and that is the rate the entry below asked for

The entry below says "whoever takes it should get the rate first — the thorough
tier (`--audio-gate N`) is the instrument". H3's session got it, and the
instrument reds on the tree it is supposed to certify.

`cargo test --test toyos-build -- --audio-gate 30` on `80fe031` — **main's tip,
no delta at all**, run as H3's A arm before that branch existed:

```
[gate A] FAILED after 15 of 30 iterations (the remaining runs cannot change this):
    pooled dropout rate: 10 of 120 vs recorded 0 of 120 (Fisher p=8.03e-4 <= 1e-3)
```

The ten, by config and iteration: `audio_tone_load smp=1` at 4, 9, 13, 15;
`audio_tone_load smp=8` at 9, 13, 14; `audio_tone smp=8` at 8, 9;
`audio_tone smp=1` at 13. So **`audio_tone` at both widths reds too**, which the
entry below had only established for `audio_tone_load`.

**The load correlation is the wrong way round, and that is the finding.** The
1-minute average across the run spanned 7.2 to 19.1 on 14 cores, with one to
five other guests and six other `toyos-build` processes throughout. The clean
early iterations ran at 19.1 and 16.8; the three worst — 13, 14 and 15 — ran at
11.4, 10.6 and 11.9. Every dropout carried a wake latency of 33-117 ms against
5-17 ms on the clean runs, which is the same "soundd was not scheduled"
signature as the two entries below.

What this changes for anyone reading them: the intermittency is not a property
of one config, and it is **large enough to fail the thorough tier's own pooled
test on a clean tree**. A stage transition that gates on this tier
(`specs/scheduler-core-spec.md` §11, and H3 itself) cannot presently tell its
own change from this. H3 therefore compared its two arms against *each other*
rather than against the recorded sample, and said so.

The recorded sample in `tests/audio-baseline.toml` is 0/120 and was taken in a
session this host no longer resembles. **Re-recording it is not licensed by this
entry** — a baseline widened to accept the defect is the defect made permanent.
What is needed is the cause.

**The B arm was never obtainable, and the reason is §3's shootdown deadlock.**
Two attempts on the audio branch stopped at iterations 2 and 4, both on
`audio_tone.smp8`, both with the tier's "instrument broken" verdict — which is
what a guest whose kernel double-panicked looks like from here. Those commits
landed between the two arms and `--land` merged them in, so the arms differ by
more than the change under test and no comparison between them means anything.
What H3 has instead: a full suite green at 289/289 with all four audio configs
clean, and ten standalone runs of the audio family. None of that is a rate.

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

**`smp=8` is not exempt — 2026-08-07, task #58's session.** It failed the same
two-boot rule twice, on a tree whose only kernel delta was the MSI-X unification
(boot-time register programming; the per-period path is untouched). A/B in one
session, `cargo test -- audio_tone_load`, HEAD against `main`'s tip merged into
it:

```
branch, in a full suite  RED  smp=8  1 [1p]/1118, then 1 [2p]/1124   wake_lat 76124us then 44159us
branch, alone            RED  both   smp=8 1 [3p]/1138, then 3/1131  wake_lat 102055us then 296659us
branch, alone × 7        GREEN both widths                            wake_lat 6543-45930us
main,   alone × 3        GREEN both widths                            wake_lat 6556-54197us
```

Both reds coincided with another worktree's suite holding all twelve guest
slots, and both carried wake latencies of 76-297 ms where every green run on
either tree sat at 6.5-46 ms — soundd not being scheduled, rather than a cost
per period. That is the same signature as the entry below and adds nothing to
the diagnosis; what it adds is that **the smp=8 config reds too**, so a future
A/B may not treat it as the quiet control.

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

### OPEN — the first gate A run to record its host: four of six boots outside the recorded sample, two of them harm

2026-08-07, tree `4a0a07f`, the run that verified the host-conditions
annotation itself (`cargo test -- audio_tone`, filtered). Every line below is
the harness's own, printed beside the counters it qualifies:

```
audio_tone      smp=1  gaps 1 [3p]  underruns 3/1137  drains 8  wake_lat 86862us (3.74 pl)  host: load 49.8/22.7/15.0 qemu 1 toyos-build 4
        confirm        gaps none    underruns 0/1136  drains 0  wake_lat 16968us (0.73 pl)  host: load 49.0/23.4/15.4 qemu 1 toyos-build 4
audio_tone      smp=8  gaps 1 [1p]  underruns 1/1113  drains 0  wake_lat 28118us (1.21 pl)  host: load 48.3/24.1/15.7 qemu 1 toyos-build 4
        confirm        gaps none    underruns 0/1111  drains 0  wake_lat  8434us (0.36 pl)  host: load 46.5/24.1/15.8 qemu 1 toyos-build 3
audio_tone_load smp=1  gaps none    underruns 0/1132  drains 0  wake_lat  7174us (0.31 pl)  host: load 41.2/23.7/15.7 qemu 1 toyos-build 3
audio_tone_load smp=8  gaps none    underruns 0/1126  drains 0  wake_lat 23083us (0.99 pl)  host: load 37.9/23.6/15.8 qemu 1 toyos-build 3
```

**The invocation passed**, and correctly: harm appeared on both `audio_tone`
configs and neither reproduced on its confirming boot, which is precisely what
the two-boot rule is for. What it passed *with* is the finding.

- `audio_tone.smp1` at **86862us — 3.74 pipeline depths, 8.6x that config's
  recorded worst (10090us), and past its 56000us ceiling.** The baseline file
  records `ceiling_runs = 0` across all 120 runs of the 2026-07-31 sample; this
  is the first breach since. It came with `drains 8` — the ceiling exactly —
  three periods of silence on the wire and a 3-period gap in the capture.
- **Two of six boots passed a whole pipeline depth** (3.74 and 1.21), which the
  baseline file states no run of its 120 reached.
- `audio_tone_load.smp8` at 23083us is 2.9x its recorded worst with no harm at
  all — the "bad but real" shape the ceilings exist to admit.

**And now the conditions are on the record rather than reconstructed.** 1-minute
load 37.9-49.8 on 14 cores with three to four other `toyos-build` processes and
no other guest, against the 4.2-6.1 the 2026-07-29 ceiling derivation recorded
per run — six to twelve times it. Under the owner's ruling of 2026-08-04 that is
**not** an excuse and not grounds to re-run it away: it is a defect of the
pipeline until something shows otherwise, and it is the same shape as the two
entries above. What is new is only that the next investigation starts from a
measured host state instead of a guess.

Whoever takes it: the thorough tier is the instrument for the rate, and it now
prints `host conditions over N runs` so its own arm's conditions can be stated.
The recorded arm's cannot — see `tests/audio-baseline.toml`.

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

### OPEN (#172) — the null sink's mix loop applies exactly one connect and then stops, on the T14 only

Read off two T14 boots (`boot7-usba-doom.log`, `boot5-doom-wedge.log`). The
shape is the same in both and it is narrow:

- soundd finds no device and presents the null sink; `null sink idle` prints.
- The first client connects. The control thread prints `opening stream`, the
  **mix loop** prints `client 0 connected (id=0)` — so it woke, drained the
  command pipe and applied the command.
- Nothing from the mix loop ever again. `stats.report` fires every 2 s while a
  client is present and the connect resets that window, so a single missing
  stats line is proof the loop stopped; boot7's client was a 2 s tone and no
  line follows it.
- Every later client is stranded: the control thread keeps running (it printed
  `opening stream` for doom 14 s later) and `open_stream` answers, but no
  `client N connected` follows, so the mixer never signals it and it blocks
  reading its signal pipe forever. That is `tone` never exiting and doom
  wedging with a black window before its render loop.

**Not reproduced in QEMU.** `tests/desktopaudiocase` was built for this: the
T14's shape, with the client's three descriptors as pipes to a terminal that is
a compositor surface — the fidelity gap `metal_sim_null_audio` and
`null_sink_shipped_client` both have, since both spawn the client from a test
binary whose stdio is the console. Green at `smp=2` and at `smp=8` (the T14's
count), with one client, with two overlapping clients under two terminals, and
with a terminal opened afterwards.

**Eliminated, each with a run behind it.** CPU count. The cpal client path
(`null_sink_shipped_client`, adopted from `wt/toyos-hdaprobe` `fa47241`, two
`/bin/tone` in series at 1.16 s and 1.15 s). soundd blocking on a client
(`signal_clients` uses `write_nonblock`; there is no blocking write in the mix
loop). The accept path being held by a stuck client (accept and mix are
separate threads and the control thread ran for 14 s afterwards). A CQ overflow
(`Poller::wait`'s `dropped` assertion would have killed soundd, and soundd is
alive). A panic anywhere in soundd, for the same reason.

**Eliminated by reading, and recorded so it is not re-derived.** The mix loop
holds no lock across its wait, and neither `PIPES` nor `IO_URINGS` was held on
the T14 at 47 s — the control thread took both to accept doom's connection and
open its stream. The mixer's timeout while streaming is finite (one device
period), so a park with that deadline is the only way to stop, and the timer
that ends it is the same one the compositor's frame interval rides. The
`remove_fd` lost wake closed above is *not* this: the mix loop's only
registration is on the command pipe, which soundd owns both ends of and nobody
closes.

**What settles it on the next T14 boot: Ctrl+Alt+D on the wedged machine.** The
dump is machine-wide and process-named now (§5), so one press answers the split
directly. soundd's mix thread parked with a sane deadline says the timer did
not fire; parked with an absurd one says the timeout was computed wrong; absent
from every parked list says nothing ever held it, which would move this into
#142's family rather than audio's. The report paints the panel, so the machine
with no serial port answers on glass and a photograph is enough.

Until that press happens, the three gates this task landed are what stands
between the milestone and a silent recurrence: a client through the null sink
must exit, a second must be taken up while the first streams, and the desktop
must still answer afterwards.

### OPEN — a desktop session put 26 ms of silence on the wire, and gate A has never measured this workload

The owner ran `cargo run` on 2026-08-07 (guest RTC 13:14:51 UTC, tree at or
after `43ce73e`), started doom from the terminal, let its demo loop run to
t=69 s, then ran `tone` 44 times. 391 s of serial, 119 soundd stats windows.
Every number below is from that capture.

**Harm, by the fast tier's own definition.** Two windows during doom:

```
soundd: wakes=537 completions=675 submitted=675 underruns=8 drains=1 max_wake_lat_us=86779 max_batch=8 clients=1 deferred=33
soundd: wakes=389 completions=690 submitted=690 underruns=1 drains=2 max_wake_lat_us=92175 max_batch=8 clients=1 deferred=7
```

Nine periods — 26 ms — submitted with a client streaming and no client audio
behind them (`main.rs:954`). `tests/audio-baseline.toml` records `underruns` 0
on all 120 runs of its sample, and the fast tier's verdict is exactly this
counter. There is no capture to corroborate it: `--dump-audio` was not on.

**And it is not confined to those two windows.** Across the whole run, 15 of
119 windows report `drains` (22 events; recorded sample 0/120), and the
`max_wake_lat_us` distribution never once enters the recorded range:

```
                     n     min     p50     p90     max
doom phase          31   21167   30367       —  106654   (4.59 pipeline depths)
tone phase          88   18116   21896   24079   63664
audio_tone sample   30    5666       —       —   10090   (baseline file)
```

The tone phase is 88 windows, none below 18116 us, against a recorded sample
whose *worst of 30* is 10090. The two distributions are disjoint. 106654 us is
past the `audio_tone_load.smp8` ceiling of 80000 (this guest is `--smp 8`).

**Whose lateness it is, is not the same question in the two phases, and the
`deferred` column separates them.** `deferred` counts a mix cycle declining to
submit because a streaming client's ring was empty and there was still playout
margin (`main.rs:894-901`) — soundd's restraint, waiting for a producer:

```
doom phase   173 deferrals across 14 of 31 windows
tone phase     1 deferral  across  1 of 88 windows
```

Same soundd, same device, same kernel. So the doom-phase underruns are **doom
failing to fill its ring**, held off by soundd until the floor and then paid in
silence — not the audio path being late. The 92175 us window sits beside a
compositor window of `frames=23` where the steady state is 65-70, so doom
stopped presenting at the same moment it stopped producing; both recovered
within one window. `tone` is a trivial producer and never does this.

That leaves the tone phase as the clean signal, and it has `deferred` 1,
`drains` 3 and `underruns` 0 across 88 windows — nothing wrong with the audio
at all, and a wake-lateness figure 2-4x the recorded sample anyway. Which is
why the measurement itself is the first thing to rule out.

**Two things make this different from the three entries above**, which are all
gate A reddening on its own configuration:

- The client is doom — a real producer with a SoundFont synthesizer thread —
  and there is a compositor blitting 200-450 MB/s to the scanout beside it.
  Nothing in gate A resembles that.
- The 44 `tone` runs exercise **suspend → resume**, 44 times. Gate A's single
  client never leaves and comes back, so the resume path has no coverage at
  all. Every resume here costs a ~22 ms lateness sample, and 22 ms is 0.94 of
  one pipeline depth (23219 us), which is what a wake measured against a
  prediction one whole pipeline stale would look like.

**Read the tone-phase cluster carefully before treating it as load.** It is far
too tight to be scheduling noise (min 18116, p50 21896, p90 24079 over 88
windows, one per stream start). One candidate mechanism, offered as a
hypothesis and not a measurement: `signal_clients`' caller arms its wait on
`target` — the *next future* grid point when the DLL estimate is past due
(`userland/soundd/src/main.rs:773-783`) — but records `armed_on = Some(t_est)`,
the stale estimate. `lateness` at :827 is then measured from an instant soundd
deliberately did not ask to be woken at, and includes every whole period it
skipped. The comment at :819-825 says the sample is taken "against the
prediction this wait was *armed* on"; `target` is what it was armed on.
Whoever takes this should settle that before reading any of the numbers above
as a property of the scheduler.

**Reproduction.** `cargo run`, `doom` in the terminal, wait for the demo loop,
quit, then `tone` repeatedly. Add `--dump-audio` so the wav can corroborate the
underruns. The counters print every 2 s while a client exists.

### OPEN — soundd reports a clean client exit as a death, 5 times in 44

Same capture. `tone` exits with `code=0` every time, and 5 of the 44 runs
produced:

```
[kernel 164.648 cpu6 tid=0] exit: tone pid=21 code=0 cpu=45ms
soundd: client 15 died, ramping down
soundd: client 15 removed
```

Clients 15, 17, 25, 34 and 39; the other 39 print only `removed`. The condition
is genuine — `signal_clients` (`main.rs:594-605`) got `NotFound` from the
signal pipe because the client process was gone — but it is a *race between
soundd's own two detection paths*, not a crash: whether the broken pipe or the
control thread's `RemoveClient` arrives first. Both set `pending_removal` and
start the same ramp, so no audio differs.

What is wrong is the word. §5.7's crash detector cannot distinguish a crash
from a clean exit that raced it, so "died" is a false positive at 11% of normal
disconnects — and it is the line an operator or a test would grep for. A
disconnect the control connection has *already* announced is knowable: the
control thread saw the peer close before the pipe broke.

Cosmetic today. It stops being cosmetic the moment anything gates on it.

---

## 5. Diagnostics

### There is no cyclictest, so nobody can ask this machine what its wake latency is

`grep -rni cyclictest` over the tree returns 12 hits, all in two spec files and
none in code: `specs/metal-boot-plan.md:350-351` ("A real cyclictest-equivalent
for ToyOS should exist before the first metal boot — it is the instrument that
turns the boot into a measurement") and `specs/production-audio-baselines.md:343-347`
and `:667-670`, which state the design — an RT-priority thread that arms an
absolute timer, sleeps, and histograms `actual − programmed` at 1 µs resolution —
and the consequence: "Until such a tool exists, **no honest 2x claim can be made
on this metric in either direction**." That is CLAUDE.md's hard bar, unmeasurable
for scheduling.

**What exists is not a substitute, and each instrument fails differently.**
soundd's `max_wake_lat_ns` (`userland/soundd/src/main.rs:995-996`, the null-sink
copy at `:1371-1372`) is read by gate A (`tests/common/audio.rs:478-488`,
`:636-645`) and baselined in `tests/audio-baseline.toml:18-22`; the thorough tier
runs Mann-Whitney on `max_wake_lat_us` (`tests/toyos.rs:1668`). But it is a
**max over a ~2 s window, not a distribution** — no percentiles, no sample count;
it measures against a DLL's *prediction of a DMA completion*, not against a
programmed timer, so it folds in the device model; and it needs soundd plus a
sound card to exist at all, which is exactly what the T14 has not got.
`toyos-sched`'s invariant I4 (`specs/scheduler-core-spec.md:1043`) bounds the
same quantity but is marked `sim`, so it can never see TCG distortion, real IPI
delivery, or metal.

**One concrete blocker before it can be written.** `SYS_SET_RT_PRIORITY` is
gated at its dispatch site on owning an audio device claim — `PermissionDenied`
unless the caller owns `VirtioSound` or `HdaAudio`
(`kernel/src/arch/syscall.rs:684-689`), whose own comment says "Spec §9.4 wants
a privilege; a claim is not one". A standalone latency tool cannot reach the RT
band today without also taking the sound card away from soundd, which changes
the machine it is trying to measure.

### CLOSED — `screen_blocked_dump`'s red, and the three diagnoses of it that were wrong

```
FAIL screen_blocked_dump: the panel does not carry "cpu(s) answered", so a
     photograph of this machine answers nothing
  ALONE screen_blocked_dump: GREEN — it fails only beside other guests, so its
        Sched::Parallel is wrong. The run stays red on the classification.
```

**It was never about the wide phase, and `Sched::Serial` was never the fix.**
Measured 2026-08-08 with the host to itself: 1 red of 4, then 1 of 5, then 1 of
6 — the same ~20% a second agent got independently from eight isolated runs at
`--jobs 1` (six green, two red, on a branch whose non-`.md` diff against `main`
is empty). The harness's own `ALONE` verdict is a coin toss on this test, so it
classified one defect as "parallel" once and "real" the next time.

**Three causes were recorded before anyone decoded the panel, and all three were
wrong about the mechanism.**

- *`Sched::Parallel` is wrong.* It reds at the same rate alone.
- *The ring tail overruns the summary.* It cannot: `Page::Last` shows the
  *newest* rows, so lines arriving after the dump would have to number sixty
  before they displaced `cpu(s) answered`. The tail was a real defect for a
  different reason, below.
- *The panic console's 3 s deadline advanced the page.* `page_forever` is
  reachable only from `halt_all_cpus` (`arch/apic.rs:237`), so Ctrl+Alt+D never
  enters the pager and nothing advances anything. `[page 3/3]` on the panel is
  the **static footer** of one `Page::Last` paint, which passes `shown = pages`
  and therefore always reads N/N. It looks exactly like a pager that has reached
  its last page and is not one.

**What it was**, from a captured red decoded pixel by pixel: the kernel painted
the whole report, and then **userland painted over it**. The band that lost its
text held the compositor's own antialiased greys — `[66,66,66]`, `[191,191,191]`,
`[237,237,237]` — and this console writes only `0x00` and `0xFF`, so those
pixels are nobody's but a client's. What survived was a 40-pixel strip beside
the window and the four rows below it. `== VERDICT:` was in the second group and
`cpu(s) answered` was not, which is why that one string of the three was always
the one that failed — the assertion was reporting *where on the glass* a line
sat, not what the kernel wrote. Two screendumps 1.5 s apart were byte-identical,
so it was a settled overwrite and not a torn capture.

The tail was a second, independent defect: pagination ran over 32 KiB of *ring*,
so the report came out as page 3 of 3 of a boot log — on one capture 31 of 67
rows were ELF relocation counts — with the answer wherever it happened to land.

Both are fixed. `log_ring::mark`/`peek_range` bracket the report, so the panel
carries one report, it fits a screen and the footer is gone;
`panic_console::hold_report` puts it back for 15 s whenever the panel stops
carrying it. Teeth, run rather than argued: `REPORT_HOLD_NS = 0` reds 3 of 3,
`peek_tail` in place of `peek_range` reds 3 of 3 on `[page 7/7]`.

**Reusable.** `[page n/m]` in a screendump is not evidence that anything paged,
and a screendump is not a shutter — QEMU converts the panel while the guest
draws on it, so a capture taken across a paint carries the rows already drawn
and nobody's rows for the rest. A predicate satisfied by one string of three
accepts one of those and then asserts on the missing half. Decode the colours
before naming a cause: all three wrong diagnoses were consistent with the
decoded text and none survived one look at the pixels.

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

### CLOSED for the dump — Ctrl+Alt+D reads the whole machine and names processes

A `CpuSched` is `!Sync` and reachable only from its own CPU, so walking a
sibling's queues is unwritable rather than racy. `task_cpu_ns` and
`task_sched_state` were rebuilt on values the owning CPU *publishes* —
`TaskHandle`'s counters, republished at each end of a pass, plus the core's
rendezvous word — so they are accurate and lock-free, which also closes the old
`try_lock`-and-skip misreport. `dump_blocked` had no such substitute: it printed
the calling CPU's parked map alone, by `TaskKey` and `WaitClass`, with no
process name, so it could confirm a park and never rule one out.

`kernel/src/sched/dump.rs` replaces it, built for #172 and for #142's family.
It walks nothing remote: the asking CPU marks every sibling, kicks it, and each
prints its own tasks from `drain_irqs` at the top of its next pass. **A CPU that
does not reach a pass inside 250 ms is named, and that is a finding** — it is
the only way this report can say a CPU is not scheduling at all.

Two halves, because neither sees what the other does. The CPUs give the
deadline, the wait class and how long the park has lasted; the process table
gives every thread's name and its published state, which is the only place a
task **no CPU was ever given** appears at all. `Ready` with `cpu_ns == 0` is
#142's signature, and the summary's `unheld` count is what the state words claim
minus what the CPUs hold.

The three failure modes degrade rather than stop the report: a silent CPU is
named and the verdict says its counts are of what answered; the process table is
retried for 20 ms and then reported as held, with the deadline half of the
verdict still printed; and a CPU inside a pass says so. Nothing allocates,
nothing waits on a lock it could find held, and truncation takes only ordinary
lines — an overdue or absurd deadline is never dropped.

It ends by painting the panel (`panic_console::paint_report`), taking the screen
from whoever holds it, because the machine it is for has no serial port and a
wedged one may never flush its log file. The verdict is the last line printed
and the console paints the newest page, so the screenful a phone camera catches
is the one carrying it.

Gates: `blocked_dump` (eight CPUs, every one present, and the two halves' counts
must agree) and `screen_blocked_dump` (muted metal-sim, a compositor holding the
screen, the verdict read back off the decoded panel). Negative controls run:
without the per-CPU answer the report reaches 1 of 8, and with the userland
check restored on the paint the panel carries nothing.

**A deadline that has passed and whose pass has not yet run is a real,
microsecond-wide state** — `blocked_dump` has seen `1 OVERDUE` on a healthy
guest. The count is evidence, not a verdict; what condemns a machine is one that
stays.

**KNOWN BLIND SPOT: the dump cannot fire when no CPU reaches a scheduler pass.**
It is dispatched from `drain_irqs` at the top of a pass, so a *partial* wedge
answers and a *total* freeze is silent — the owner pressed it after pulling the
USB stick and got nothing, which is itself weak evidence that no CPU was
passing. Whether an interrupt-dispatched variant can close that is the
scheduler agent's (the NMI-on-timeout proposal); what follows is what the
author of this facility established while building it, so nobody rediscovers it
or removes it by accident.

- **`Lock::lock` disables preemption, not interrupts** (`sync.rs`) — a ticket
  spin after `preempt::disable()`, no `cli`.
- **That fact is load-bearing for the 250 ms sibling wait.** Spinning there is
  safe *because a preempt-driven pass provably holds no `Lock`*: taking one
  raises the preempt count, so a pass cannot be entered while one is held.
  **The argument does not survive being moved to an interrupt**, which can land
  on a CPU that *is* holding a lock — the waiter would then block the sibling
  it is waiting for. `request`'s `assert!(depth <= 1)` encodes exactly this
  entry condition and is not decoration.
- **`log!` takes the serial lock.** It is the single reason the report is not
  ISR-safe today: an interrupt landing on a CPU that holds it deadlocks on the
  first line. The panic console's paint is the lock-free counter-example and is
  ISR-tolerable; `paint_report`'s `PAINTING` is a swap latch that self-releases,
  not a `Lock`.
- **The per-CPU half is already reentrancy-safe by construction.** The
  schedulers are not behind a `Lock` at all: `SCHEDS[cpu]` is guarded by the
  `IN_PASS[cpu]` flag, mutation happens only inside `with_cpu` which sets it,
  and `try_with_cpu` refuses when it is set. An interrupt that lands mid-pass
  therefore reads nothing rather than reading a torn map.
- **The process table is `try_lock` retried to a 20 ms ceiling, and the retry
  is not belt-and-braces.** Bare `try_lock` was the first version and
  `screen_blocked_dump` caught it losing the whole census to a *transient*
  holder — a spawn in flight 0.75 s into boot. The ceiling separates "someone
  is mid-spawn" (microseconds) from "the holder is what is wedged" (never).
- **Truncation may never hide what the report is for.** `LINES_PER_CPU` bounds
  ordinary parked lines; a line whose verdict is `Overdue` or `Absurd` is not
  counted against the budget, and `UNPRINTED` says how many ordinary ones went.
- **The summary is last for a causal reason, not a cosmetic one**: it is the
  only part that needs every CPU to have answered, and the console paints the
  *newest* page, so last is what a photograph catches. Any variant that
  reprints must keep that order.
- **The deadline arithmetic is guarded by its own classification.**
  `Verdict::of` matches `at <= now` before it ever evaluates `at - now`, and
  `Deadline`'s `Display` only subtracts in the arms that verdict produced.
  Overflow checks are on in the kernel, so reordering those arms turns a
  diagnostic into a panic.

What is left of this entry: `ps` and `stats` still have no cross-CPU view of
anything the handles do not publish.

#### CLOSED — `screen_blocked_dump` is intermittent *alone*

The rate is right — five runs a side on 2026-08-07 gave three green and two red
on `main` at `48147c2` and the same on `wt/toyos-wedge`, so it reds with the
host to itself and at the same rate on an untouched tree. The mechanism recorded
here (the ring tail lifting the summary off the page) was wrong, and §5's entry
above has what it was: userland repaints over the report, and the failing string
is the one that sat under the window rather than beside or below it.

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

### OPEN — the syscall profile is 64 bins wide and the ABI reaches 96, so every audio, net, IPC and pipe call is missing from it

`ProcessData::syscall_counts` is `[u32; 64]` (`kernel/src/process.rs:552`) and
`syscall_dispatch` guards the bump with `if (num as usize) <
data.syscall_counts.len()` (`kernel/src/arch/syscall.rs:218`). `syscall_total`
at :217 is bumped unconditionally. `SYS_MMAP` is 63 — the last bin — and
`SYS_MUNMAP` is 64, the first one dropped. Everything above it goes with it:
`SYS_AUDIO_SUBMIT` 71 and `SYS_AUDIO_POLL` 84, `SYS_NIC_{RX_POLL,RX_DONE,TX}`
78-80, `SYS_LISTEN`/`ACCEPT`/`CONNECT` 85-87, `SYS_PIPE_{OPEN,ID,MAP}`,
`SYS_READ_NONBLOCK`/`WRITE_NONBLOCK`, `SYS_IO_URING_{SETUP,ENTER}`,
`SYS_EXIT`, `SYS_PROCESS_STATS`, `SYS_SET_RT_PRIORITY`.

The line therefore prints a total that is not the sum of its parts, silently.
From the 2026-08-07 desktop capture, doom:

```
syscalls: pid=6 total=33190 syscall_wall=3806ms 0=6129 1=4585 6=1 8=14727 9=4 10=3 13=891 14=3 38=4 39=1 40=2 41=2 49=1919 53=2 59=5 63=17
```

The bins sum to 28295 against `total=33190` — **4895 calls, 15% of the process,
invisible**. Every one of the 44 `tone` processes in the same capture reports
`total=28` with bins summing to 22; the six missing are its whole reason for
existing (connect to soundd, map the ring, exit).

Reproduce with any process that makes an audio or network call:

```
grep 'syscalls: pid=' <log> | awk '{t=0;s=0; for(i=1;i<=NF;i++){ if($i~/^total=/){split($i,a,"=");t=a[2]} else if($i~/^[0-9]+=[0-9]+$/){split($i,b,"=");s+=b[2]} } print t, s, t-s}'
```

This is the diagnostics roadmap's layer 1 — the layer that exists to answer
"where is time going" — and it cannot see the audio path at all. Whatever the
fix (size the array from the ABI's highest number, or make the array
`[u32; SYS_MAX]` with a compile-time bound), the silent `if <` is the defect:
a bin that cannot hold a number should refuse it by name or not exist.

### OPEN — `exit: <name> pid=N cpu=Xms` is the main thread's CPU, not the process's, and the two lines look identical

`teardown_bookkeeping` prints `cpu={main_cpu_ns}` (`kernel/src/process.rs:1037,
1040`) — the main thread's scheduler total, the same value
`stash_accounting_snapshot` then *adds* `child_threads_cpu_ns` to at :1082
before handing it to `waitpid`. The per-thread line at :1230 has the same shape
and prints that thread's total. Nothing distinguishes them but `pid=` versus
`tid=`.

The result is a process that used less CPU than one of its own threads. From
the 2026-08-07 capture, 43 of the 44 `tone` runs read this way:

```
[kernel  87.632 cpu5 tid=1] exit: tone tid=1 code=0 cpu=121ms
[kernel  87.634 cpu4 tid=0] exit: tone pid=8  code=0 cpu=46ms
```

and doom's three lines (`tid=1` 3409 ms, `tid=2` 14759 ms, `pid=6` 59050 ms)
cannot be added, subtracted or reconciled by a reader who does not have
`process.rs` open. The whole-process figure exists — `waitpid` gets it — and is
the one number the log does not carry.

### OPEN — the serial console has no line atomicity, so a stats line can be cut in half by another writer

std's `Stderr` is unbuffered, so one `eprintln!` reaches `SYS_WRITE` as several
calls, one per format fragment. `SerialWriter::console` buffers a *single*
write and commits on drop (`kernel/src/drivers/serial.rs:303-319`), which makes
each fragment atomic and a line not. Two instances in one 2110-line capture,
2026-08-07:

```
netd: MAC soundd: ready, 52:54:8 buffers, 00:12:34:56
44100Hz 2ch, 512 bytes/period, 128 frames/period
netd: ready, at most 42 piped connections (4 MiB each of 1356 MiB total)
```

```
soundd: client [kernel 351.736 cpu0 tid=0] syscalls: pid=46 total=28 …
40 removed
```

The second is the kernel's own ring landing inside a userland line, so this is
not only a userland-vs-userland race. On the T14 the log *is* the instrument,
and this capture shows what that costs. Counting soundd's client lifecycle in
it gives 45 connects and 44 removes, and the one id with no removal is 40 —
whose removal is the split line above. A reader auditing that log for leaked
clients finds one, and it is not there:

```
$ grep -c 'connected (id='            45
$ grep -cE '^soundd: client [0-9]+ removed$'   44
```

Cheap in principle: give the toyos `Stderr` a line buffer, or have
`write_user` hold the ring across one logical line. Neither is free of
tradeoffs — a line buffer changes flush ordering against the kernel's ring —
so it is filed rather than fixed.

**It reds landings, which is new.** `landing-1786130703-71774.log` (2026-08-07
21:32) failed the gate on a **documentation-only branch**: `hda_tone` wants
`soundd: hda codec0 vendor=1af4` and the console carried

```
soundd: hda codec[kernel 0.262 cpu1] i8042: armed at 184ms, idle at 262ms, …
0 vendor=1af4 device=0012, 1 function group(s)
```

The splice fell between `codec` and `0`. `Serial::interleaved` named it in the
failure message, which is that instrument working — but the run is still red,
and re-running is the only recourse. soundd's next two needles a few
milliseconds later matched intact, so which line is hit is chance, and every
`must_say` naming userland output carries this rate.

That is a **third** fix candidate, harness-side and free of the flush-ordering
tradeoff the two above carry: a kernel line is *inserted* into the byte
stream, and `is_kernel_line` already identifies it, so `Serial` can splice the
fragments either side of one back together and match the needle against the
stream userland actually wrote. It repairs the gate, not the log — a human
grepping the T14's log still sees the split line, which is what the two
guest-side fixes are for.

Do not instead shorten needles to fit inside a fragment. The splice point
moves, and a needle short enough to survive every splice is short enough to
match the wrong line.

### OPEN — the i8042 aux line's unmask result is discarded, and its log line says nothing either way

`init` captures the keyboard's unmask and prints it — `"on"` or `"MASKED"`
(`kernel/src/drivers/i8042/mod.rs:1527, 1544`). The aux line one statement
later is `let _ = ioapic::set_masked(l.gsi, false);` (:1529), and its log line
(:1547) reports the GSI, the vector and the APIC and stops:

```
i8042: kbd set2+xlat (readback 0x41) scanning on, GSI 1 -> vec 0x24 apic 0 on
i8042: aux rate=100 res=8/mm, GSI 12 -> vec 0x24 apic 0
```

An aux GSI that failed to unmask prints exactly that line. On the T14 that is
the TrackPoint and touchpad silently dead with a green-looking boot — the
failure mode the comment 30 lines above (:1494-1497, "every line below prints
green") was written to prevent, reached by the one path that does not check.
Not observed failing: this capture is QEMU with USB HID, and `i8042: armed at
191ms, idle at 335ms, 0 interrupts` is expected there. The point is that the
log cannot tell you which it was.

Trivial to fix the same way the kbd line already is.

### CLOSED — `screen_blocked_dump` asserts a whole-report property against one page of a paged report

Red about a quarter of the time, on `main`'s own code. Found 2026-08-08 by a
branch whose entire delta is three `.md` files — `git diff main...HEAD --
':(exclude)*.md'` is empty, so the kernel, bootloader and initrd it booted are
main's byte for byte, and no A/B against a second tree is needed to say so.

**Rate, measured.** Eight isolated runs (`--jobs 1`, nothing else on the host):
six green, two red. Two landing gates on the same branch red on it in the wide
phase; the harness re-ran it alone both times and got `GREEN` once and
`red again — the defect is real` once. So the `ALONE` verdict itself is a coin
toss here, which is worth knowing before anyone trusts one.

**Cause, from a captured red.** `tests/toyos.rs` required three strings on one
screendump — `"== VERDICT:"`, `"cpu(s) answered"`, `"== deadlines:"` — and the
red capture ended:

```
[kernel 0.601 cpu0] == VERDICT: 1 overdue, 0 absurd, 0 unheld, 0 never ran
[kernel 0.601 cpu0] === end of dump ===

[page 3/3]
```

**The conclusion drawn from that footer was right and the mechanism was not.**
Right: the report was three pages of a boot log and the answer was not all on
one, which is a defect however the panel got there. Wrong: nothing was advancing
those pages. `page_forever` — the 3 s deadline, PageUp/PageDown, the first key
retiring it — is reachable only from `halt_all_cpus` (`arch/apic.rs:237`), so
Ctrl+Alt+D never enters it. `[page 3/3]` is the *static footer* of a single
`Page::Last` paint, which passes `shown = pages` and so always reads N/N. It is
indistinguishable on a photograph from a pager that has reached its last page,
and it is not one.

The note not to widen the deadline stands and was never reachable anyway. What
the report is paginated *against* was the real half: 32 KiB of shared ring
rather than the report. Both that and the overwrite that hid it are fixed and
written up in §5 — `log_ring::mark`/`peek_range` and
`panic_console::hold_report`. The assertion was not weakened: it now also
requires the absence of a `[page n/m]` footer, because Ctrl+Alt+D paints once
and a report needing two pages leaves the answer on one nobody can reach.

---

## 6. Build and toolchain

### The boot capture landed, and two doc comments still tell agents it did not

The capability is there and is used: `QemuInstance::boot_log` (`tests/common/qemu.rs:1251`,
accessor at `:1548-1558`) holds everything `wait_for_ready` saw on the way to the
ready marker (`:2494-2583`, accumulated into `seen` on both arms and returned at
`:2468-2472`), and `tests/common/serial.rs:41-43` wraps it as `Serial::boot` with
`must_say`/`must_not_say`/`must_be_clean`/`alive`. Counted with
`grep -rn 'boot_log()' tests/ | wc -l` and its two siblings: **68** `boot_log()`
call sites, **13** `Serial::boot`, **131** `must_say`/`must_not_say`/`must_be_clean`
outside the helper. `tests/common/faults.rs:221-242` asserts on
`VirtIO net: NOT INITIALISED at PCI` and four more boot lines; `metal_sim_scanout_wc`
reads its PAT and MTRR lines out of a shared boot's seeded console
(`tests/toyos.rs:3530`, `:3841-3893`). It arrived in `8025f57` (2026-07-31) and
`a1ef357` (2026-08-01), both ancestors of `main`.

**What is left is narrower and real.** `TestResult.serial` still starts at
`===TEST_START` — `tests/common/qemu.rs:1747-1748` sets `in_test` there and
nothing is appended before it — so gate A's `serial`, built at
`tests/toyos.rs:1283` as `result.serial + &qemu.drain_serial(500ms)`, carries no
boot prefix, and `audio::check_suspend_structure` (called at `tests/toyos.rs:1409`)
therefore still cannot see a device started before the window opened. That is
one line to fix now that the capture exists: prepend `qemu.boot_log()`.

**And two comments now mislead about the harness, which is why this reads as
open.** `tests/common/audio.rs:519-531` says the harness "never joins the reader
thread's full log … Catching that needs the boot capture the harness currently
throws away" — it does not throw it away. `tests/toyos.rs:1022-1024` says a
device started before `===TEST_START` is "invisible here as it is everywhere
else" — it is not invisible everywhere else. One genuine blind spot survives by
design: `BootOptions::mute` sets `boot_log` to `String::new()`
(`tests/common/qemu.rs:2468-2470`), and `Serial::alive()` exists so an assertion
over an empty capture fails rather than passing vacuously.

### The kernel's crate-level `allow(dead_code)` hides 49 warnings from the zero-warning bar

`kernel/src/main.rs:3` is `#![allow(dead_code)]`. `kernel/.cargo/config.toml`
carries `-Dwarnings`, so the kernel is warning-clean today *because of* that one
line rather than in spite of it.

Measured by reproducing `src/build.rs:209-256`'s invocation as a `cargo check`
into a scratch `CARGO_TARGET_DIR`, so neither the worktree nor the shared sysroot
was touched:

```
cd kernel && RUSTUP_TOOLCHAIN=toyos \
  RUSTFLAGS='--force-warn dead_code -Cforce-frame-pointers=yes' \
  CARGO_TARGET_DIR=<scratch> cargo check --target x86_64-unknown-none \
  --profile toyos --message-format=json
```

**71 `dead_code` messages, 65 of them the kernel's** (3 `bcachefs`, 3
`hashbrown`); the same command without `--force-warn` gives 0. `--force-warn`
overrides item-level allows too, and four of those exist in the kernel
(`arch/mod.rs:3` on `mod debug`, `main.rs:42` on `mod vfs`, `id_map.rs:92`,
`drivers/virtio.rs:73`), accounting for 16 of the 65. **So deleting `main.rs:3`
alone surfaces 49.** They are unused constants, never-read fields and unreachable
methods — `PORTSC_CCS/PED/PR/SPEED`, `CMD_GET_DISPLAY_INFO`, `DR7_*`,
`struct DisplayOne`, `static NEXT_CPU_ID`, `map_alloc`, `is_mapped`,
`clear_regions`, `unmap_2m`, `wake_one`, `port_bit`, `redirection`. Default
features only; an actuator feature could move the number.

Same family as *The fork estate is invisible to the zero-warning bar* below: a
bar with a crate-level exemption under it certifies less than it looks like it
does, and "Dead code is deleted" is a stated principle.

### `wall_clock_refusals` is five boots in one registration, and can be the longest job in the parallel phase

`tests/toyos.rs:465` registers it `Sched::Parallel`; the body
(`tests/common/wallclock.rs:281-305`) calls five helpers in sequence and each one
goes through `boot_and_read` (`:123-176`), which builds an image and boots one
guest. Five distinct kernel builds: `rtc-dead`, `rtc-unstable`,
`rtc-no-century`+`rtc-century-next`, `rtc-century-next`, `rtc-zone-east`. None is
in `INERT_ACTUATORS` (`tests/common/qemu.rs:1315-1322`), so `fold_inert` merges
nothing — one worker takes all five, serially.

Recorded durations on disk, from other worktrees' `target/test-durations`
(this one has never been run):

| file | entries | `wall_clock_refusals` | rank | that run's next-longest |
|---|---|---|---|---|
| `toyos-h3` (2026-08-08 00:08) | 285 | **209 405 ms** | **1st** | `i8042_kbd_echo` 187 280 |
| `toyos-hdaprobe` (2026-08-06 11:47) | 258 | **18 989 ms** | 8th | `xhci_msi_only` 39 054 |

The h3 figure is from a contended run (§7's class), so "longest" is that run's
verdict and not a clean measurement; 19.0 s is the uncontended shape. Its
already-split sibling `wall_clock_file` costs 2818/3189 ms in the same two files.
`longest_first` (`tests/toyos.rs:9991`) can order jobs and can never split one,
so the ordering cannot help here.

**The split is free.** The kernel artifact memo (`src/build.rs:696-745`, keyed at
`:772` on `[PROFILE, features]`) builds one kernel per feature set per process,
so five registrations build exactly the five kernels the one registration already
builds — and the parallel phase gets five jobs it can place instead of one it
cannot.

### SMEP is on everywhere except `cargo run`, and nothing asserts it anywhere

Both halves of the old gap closed. The kernel enables it —
`arch/cpu.rs:145-176` `enable_smep` (CPUID leaf 7 EBX bit 7, then `CR4.SMEP`),
with `enable_smap` beside it at `:184-217`, called on the BSP at
`arch/percpu.rs:411-412` and on every AP at `:468-469` — and the harness passes
it, `tests/common/qemu.rs:2199` on both the KVM and TCG arms, landed in `5d53aa0`
(2026-08-06, ancestor of `main`).

Two residuals:

- **`cargo run` was never given it.** `src/qemu.rs:88` and `:90` are still
  `host,+rdrand,+smap,+fsgsbase,+x2apic` and `qemu64,+rdrand,+smap,+fsgsbase,+x2apic`.
  So the interactive path — including `--metal-sim`, whose whole purpose is to
  be the T14's shape — differs from the harness in exactly the dimension the
  harness was changed for. `grep -rn smep tests/ src/` returns one hit, the
  harness argument.
- **Nothing asserts it.** No test reads `smep=on` out of a boot log
  (`grep -rn "percpu: BSP" tests/` → nothing), so deleting the argument, or
  breaking the CPUID gate in `enable_smep`, reds nothing. Nor does any test
  execute a user page from ring 0 — and such a test would be a weak instrument
  anyway, because the kernel executes out of the direct map, which has no U bit
  (`specs/memory-boundary-spec.md` §2.1), so SMEP does not cover the kernel's own
  alias of a user page. That is #159/#166 territory, not this.

The two spec paragraphs that still described the pre-`5d53aa0` state are
corrected in this commit (`specs/memory-boundary-spec.md` §2.1 and §3.2);
§8 there is a dated record of the discovery run and its line citation is left
alone.

### The page cache owns one device, and `usb_storage.rs` says it does not

`page_cache.rs:11-12` holds exactly one
`BLOCK_DEV: Lock<Option<Box<dyn BlockDevice>>>`, `page_cache::init` takes
ownership of the NVMe device, and `PageCache::_device_id` is written at
construction and read nowhere. So `usb_storage.rs:14-17`'s comment — *"NVMe
takes 1; the page cache keys itself on this, so two devices sharing a number
would serve each other's blocks"* — describes a mechanism that does not exist.
The numbers are right and the keying is not.

`fat32_adapter.rs:911-915` states the live consequence and does not work around
it: a machine that boots off an **internal** disk gets neither `/boot` nor
`/log`, "because the NVMe device is owned by the page cache from the moment
storage comes up and there is no second handle to it". `/boot` and `/log` work
on the T14 and in QEMU only because the boot medium is USB, and
`usb_storage::open` mints a fresh handle per call.

This is the real cost of `specs/boot-image-split.md` stage 2: a bcachefs root on
the boot medium needs a `BlockIO` over an arbitrary `BlockDevice` at a partition
offset with a cache of its own, where `PageCacheBlockIO` *is* the NVMe device by
construction. Found 2026-08-07 while pricing that stage — the 2026-07-29 version
of that document listed this as one of eight items a USB storage driver would
have to bring, and it is the one that did not arrive with it.

### Nothing gates doom's music, and nothing ships a SoundFont to gate it with

The original finding: `b34a69c` filtered the asset sweep to what git tracks and
took `assets/timgm6mb.sf2` out of every image; the full suite was green with it
and without it, `doom_sound_flood` included — that actuator "synthesises its own
sound and never opens the WAD or the soundfont". The only evidence anywhere was
one `assets: skipping` line in the build output. `fdcaa0b` restored the file and
made a declared-but-absent asset a hard error, so for a while the *file* was
gated even though the music was not.

**The file is now deliberately absent and the hard error is gone** — the owner's
decision, 2026-08-08. TimGM6mb is GPL-2.0 and this tree is MIT OR Apache-2.0, so
shipping it put copyleft obligations on anyone redistributing an image; every
permissive General MIDI replacement is around six times the size of the image's
largest entry, and the image is already too big. Music is opt-in from a `.sf2`
dropped in as `assets/soundfont.sf2`; a declared entry the build cannot find is
named and skipped; the absence is stated at build time and again by
`toyos_music_init`'s "playing without music". `system.toml` names GeneralUser GS
(CC-BY-4.0) as the recommended one and records that its author cannot be certain
of every sample's origin.

**What is left is the ungated half this entry was always about, and it is now the
normal case rather than an accident.** Nothing asserts doom's synthesiser
produced anything, so a defect between a SoundFont and the sink is invisible to
`cargo test` — and on CI, on a fresh clone and in every worktree there is now no
SoundFont for such a defect to be found with at all. Gate A measures the test
tone and doom's sound-stress actuator; neither is the music path. A gate on
music would have to carry a permissively-licensed `.sf2` of its own, which is
the same licence question one size down, and that is why there is not one.

### No test boots the config the project ships

`system.toml` is what `cargo run` builds and what a stick is flashed with, and
the harness boots none of it: `tests/testcases`, `desktopcase`,
`desktopaudiocase`, `doomcase` and `metalcase` are each their own config, and
`screen_diag_boot` / `screen_console_shell` boot `diag/` and `console/`. So the
shipping image's init list, its `hosted-rustc` setting and its program list are
exercised only by the owner running `cargo run`, which agents are told not to
do. The one gate on that file is `no_shipped_boot_config_starts_sshd`, which
reads it rather than booting it.

Noticed 2026-08-07 while landing `hosted-rustc = false`: that change alters only
`system.toml`, so no suite test could go red for it in either direction.

### `debug = true` produces no debug info, because the linker drops it

`toyos-ld` matches `SectionKind::Debug | DebugString | Linker | Note | Metadata`
and `continue`s (`collect.rs:410-416`), so **no binary this project produces has
a `.debug_*` section**. Verified with `readelf -S` on the kernel, the compositor
and toybox: the sections are `.text .strtab .symtab .rela.dyn .data
.eh_frame_hdr .dynamic .shstrtab` and nothing else.

`[profile.toyos]` states `debug = true` in every crate root, so rustc emits
DWARF into every object file and the linker throws all of it away. The cost is
compile time and has not been measured. The consequence for diagnostics is that
a backtrace can carry a **name** and never a line number or an inlined frame, on
any path — `.symtab`/`.strtab` is the whole of what survives, and it is 32.2% of
the 92,138,384 bytes of ELF this tree ships. Keeping `.debug_line` in `toyos-ld`
is what would change that, and it is not planned.

### `userland/libc` is the one guest artifact built without overflow checks

`src/libc.rs` passes `--release`, so the C runtime std links into every userland
binary has `overflow-checks` and `debug-assertions` off while everything around
it has them on. Deliberate on two grounds — CLAUDE.md gives the POSIX
compatibility layer explicitly relaxed rules, and `libc::build` is gated on
`stamps::dir_changed` over the *source* directory, so changing the flag alone
would not rebuild the installed archive and the manifest would then claim
something the artifact does not have. Recorded so that "one profile, applied
consistently" is not read as covering it.

### CLOSED — `standing()` cannot tell a worktree that is *ahead* of main from one that is *behind* it

`toolchain::standing` asks `git diff --quiet main -- toyos-abi/src toyos/src
userland/libc/src` and reads a non-empty diff as `Diverged` — the standing that
entitles a checkout to `--claim-sysroot`. A diff is symmetric, so a worktree
that has simply **not merged main** since somebody landed an ABI change reads as
`Diverged` too, and may claim: it would then rebuild the shared sysroot from
sources that are *older* than main's and refuse the worktree whose change is
already landed. That is the 2026-08-04 fight §3.2 of `specs/worktrees.md` exists
to prevent, arrived at from the other direction.

Observed on 2026-08-07 during task #133, which landed `SyscallError::Io` on its
own commit for exactly this reason. Nothing went wrong that day — the worktree
that claimed had merged main *and* held a real `toyos-abi/src/ring.rs` change of
its own, so it was rightfully diverged — but its standing would have been
`Diverged` either way, and the check did not distinguish them.

The question the check means to ask is whether this checkout has content in
those trees that main does not: `git diff --quiet main...HEAD` against the merge
base, plus the working tree, rather than `git diff main`.

**Fixed 2026-08-07.** `standing()` asks `git diff --quiet main...HEAD --
<SYSROOT_SOURCES>` and, separately, `git status --porcelain --
<SYSROOT_SOURCES>`. Two questions rather than one because they are two: the
first is what this branch added and the second is what has not been committed —
and `status` rather than a second `diff`, because a *new* file in `toyos-abi/src`
changes the witness and no diff against a commit reports an untracked one, which
the old check could not see either. Gate:
`toolchain::tests::a_checkout_behind_main_has_no_standing_to_claim`, watched red
on the tree as it stood (`left: Diverged, right: MatchesMain`) and green after.
The three existing standing gates are unchanged and still green, including the
one that deletes `main` — `git diff main...HEAD` exits 128 there, which is
`Unknown` exactly as before.

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

**Read this list against `specs/test-cost-audit.md` §5.8 before adding to it.**
Every entry below that says `nothing typed at the terminal window reached a
shell` — `desktop_typing_damage`, `desktop_locale_detect`, `blocked_dump`, and
`desktop_audio_client` in the entry after this one — is now known to be the
`/bin/terminal` boot race in §3 reported through a wall-clock guard that could
say nothing else: three of three such reds in an eight-suite session carried the
race in their boot log, and the wait they blew had been ruled out at 0.6 s by
`exit: terminal pid=N code=1`. `shell_echoes` names the race now. So an
`ALONE: GREEN` beside one of those sentences was never evidence that the host
was the cause; it was evidence that the *boot* differed, which a re-run also
changes.

- **`i8042_mouse`** — CLOSED 2026-08-06. Both red modes were the harness and
  neither was ever the driver losing a packet; §8's entry carries the mechanism,
  the measurements and the two gates that now hold each half. The short version:
  the pacing lead was 32 packets — 96 bytes — against QEMU's 16-byte
  `PS2_QUEUE_SIZE`, so a host that got ahead of the guest made QEMU *sum* the
  motion it had no room to queue, and a summed pair that cancels reaches
  userland as nothing at all. The lead is now 4 packets with a `const` assert
  against the device's queue, and the lost-edge counter no longer fires on a
  pass that read the `irq_ring` record a few instructions before the ISR
  published it.
  **Not closed after all, at the count.** 2026-08-07, two full suites in one
  worktree while a second worktree held six of the twelve guest slots: `1003
  pointer events reached userland out of 1004 packets injected, never more than
  4 of them (12 bytes) outstanding against a 16-byte device queue`. The lead is
  inside the bound the fix installed, so the summing mechanism §8 describes is
  not what this is. A/B in one session, `git checkout main -- kernel/` in the
  same tree minutes apart: this branch PASS 33 s first try, **main's kernel FAIL
  with the identical 1003-of-1004 line**, then PASS 2 s on the harness's own
  re-run. So it is not a tree difference and it is not gone — one packet in a
  thousand is still being lost, or still being counted wrong, under a host
  carrying two suites.
- **`i8042_absent`** — same session, same shape, and it is `Sched::Serial`
  already, so intra-suite width is not what reaches it. The verdict is the
  guest's own `Boot: complete` on two boots with a 300 ms allowance; the landing
  gate saw `601ms without an i8042 and 287ms with one`. Alone, minutes later:
  this branch 619 vs 507 (PASS), main's kernel in the same tree 277 vs 331
  (PASS). The absolute figure moved 277→619 ms across three runs of one boot
  with no code change, so what the allowance is being asked to absorb is the
  host, and a serial slot inside one suite does not buy a quiet one.
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
  `qemu::budget(20 s)`, the phase's. Still `Sched::Parallel`. **See the entry
  below: as of 2026-08-06 this is no longer occasional but reproducible, and the
  mechanism is the duration profile.**
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
- **`dump_nmi_probe`** — added 2026-08-07, and the odd one out: it is already
  `Sched::Serial`, so it failed in the *serial tail* rather than the wide phase
  and the harness therefore never re-ran it alone. Run alone on the same tree
  moments later it passes in 23 s. `the NMI went unanswered too` is its
  wall-clock verdict expiring on a host carrying three other worktrees' suites —
  the `[host-slots]` lines in that run name all three. `4ad8875` made it serial
  for exactly this reason, which shows what serialising buys and what it does
  not: within one run the phase is quiet, across runs nothing but
  `buildlock::guest_slot` spans worktrees and twelve slots is not one guest.
  Nothing here should widen its millisecond.
- **`blocked_dump`** — added 2026-08-07, `nothing typed at the terminal window
  reached a shell`, `ALONE … GREEN` in 5 s. Same shape and same sentence as
  `desktop_typing_damage` and `desktop_locale_detect`: its verdict is the dump's
  content, but *reaching* the dump crosses a compositor, a terminal and a shell,
  and that step is a wall-clock margin. Still `Sched::Parallel`.
- **`screen_console_scroll`** — added 2026-08-07. `round 1: the guest never
  printed CHURN-DONE 0 100`, **598 s** in the wide phase, `ALONE … GREEN`. The
  landing gate it killed ran 778.9 s with four other `--land` processes on the
  host, on a branch whose whole delta was two documentation lines. 598 s against
  a phase that is ~45 s on a quiet host is the finding; the message is not.
  Still `Sched::Parallel`.
- **`hda_tone`** — added 2026-08-07, hours after the test itself landed. In a
  full run on a host carrying another worktree's suite: `2 mid-tone silences in
  the capture: total 2 [3p×1 4p×1]`, `dither 3.3%`, `phase-breaks 92`. Alone on
  the same tree eight minutes later: `gaps none`, `phase-breaks 16` — the
  declared #88 failure and nothing else. It is `Sched::Serial`, so like
  `dump_nmi_probe` the harness never re-runs it alone and the run simply reds.
  Its `EXPECTED_FAILURES` entry covers the phase-break message alone, which is
  why a *dropout* under load reaches the verdict, and that is correct: **do not
  widen it.** A silence and a phase break are two different defects and an entry
  that covered both would stop saying anything. The tree it was seen on differed
  from main only in `src/`, so the guest image was byte-identical to main's.
  **Three times the same day**, all three in landing gates of that one
  build-system branch and all three confirmed alone within ten minutes: `2
  mid-tone silences`, then `1 mid-tone silence`, `gaps none` alone every time,
  with three to five other `--land` processes on the host. Ask
  `git diff main...HEAD` and never `git diff main` when checking whether a tree
  could be the cause: the second is symmetric and lists what *main* changed since
  the branch last merged, which reads as the branch's own work and is not.

- **`xhci_hid_break`** — added 2026-08-07, in a landing gate on a branch whose
  delta since its own previous green gate was one documentation commit. `input
  never came back: no pointer event moved by (2560, -1920); deltas seen:
  [(256, 256), (256, 256)]`, `ALONE … GREEN`. The two deltas it did see are the
  boot-time absolute tablet, so what went missing is the relative mouse's event
  after the staged break — a wall-clock margin on the recovery path, not a
  recovery that failed. It is one of the three longest jobs in the suite by
  `longest_first`'s own profile, so it is dispatched early and runs beside
  everything. Still `Sched::Parallel`.

**The eight-landing regime, and what it does to the paragraph above.** That
paragraph says the four-suite regime "cannot recur" now that `guest_slot` admits
twelve guests across every worktree. It recurred on 2026-08-07: **eight
`toyos-build --land` processes were queued on the integration lock at once**, and
one branch's two consecutive landing gates died on two *different* tests from
this list — `blocked_dump`, then `screen_early_panic` — each `ALONE … GREEN`,
neither related to a branch that touched only `tests/`. The semaphore is not
wrong; it counts the thing it says it counts. But a landing gate is a full build
plus a suite, and **the build half is bounded by nothing** — eight of them is
eight cargo trees compiling on 14 cores, which reaches every liveness margin in
the wide phase without a thirteenth guest ever existing. The gate's own audio
lines recorded the host at seven `toyos-build` processes throughout.

So the closing claim needs the qualifier: guest slots bound *guests*, and a
landing storm is not made of guests. Whether the integration lock should also
gate the gate's build, or whether these tests belong in the serial tail, is a
decision for whoever owns the harness; what is established here is that a branch
can be unable to land for reasons that have nothing to do with it.

**Bounded the same day, and the count was closer to home than a landing storm.**
A worker takes a guest slot and then *compiles its kernel variant*, so twelve
workers in one suite are twelve concurrent `cargo build`s before any of them
boots — which is the load 49.9 with twelve rustc/cargo processes and exactly one
guest live that was measured while this was being written, on a host where the
semaphore was doing precisely what it says. `buildlock::build_slot` is the
second count: four across every worktree, its own directory so a suite holding
every guest slot can still compile, `--host-builds N` to override and `0` to turn
off (`specs/test-cost-audit.md` §5.7). It bounds the build half of a landing gate
by construction, since a gate's builds are these builds. What it does **not**
bound is anything that never enters `src/build.rs` — a `toyos-sched-sim measure`,
a hand-run `cargo build` in a fork clone, the primary's `./x.py`.

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

### OPEN — `desktop_window_child` holds a lane for four minutes, and whichever other desktop lands beside it loses its typing window

Measured 2026-08-06 in one worktree, seven full runs, on a host at roughly three
times its own load.

Three tests boot `tests/desktopcase` at `smp: 8` — `desktop_typing_damage`,
`desktop_audio_client`, `desktop_window_child` — and each reaches its shell
through `shell_answers`, whose retry window is `qemu::budget(20 s)`.
`desktop_window_child` is an expected failure (§3) that runs its
`close_focused_window` retry loop out to that same budget, so it occupies one
lane for **~250 s of every run**. `longest_first` orders the parallel phase on
`target/test-durations` and therefore dispatches it first; whichever of the
other two the profile ranks next goes in beside it, waits behind two eight-CPU
guests on a fourteen-core host, and reports `nothing typed at the terminal
window reached a shell`.

| run | profile | victim | wide | alone |
|---:|---|---|---:|---:|
| 1 | none (fresh worktree) | — | — | — |
| 2 | written by run 1 | `desktop_typing_damage` | 243 s FAIL | 16 s GREEN |
| 3 | " | `desktop_typing_damage` | 255 s FAIL | 16 s GREEN |
| 4 | " | `desktop_typing_damage` | 246 s FAIL | 16 s GREEN |
| 5 | deleted before this run | — | — | — |
| 6 | written by run 5 | — | — | — |
| 7 | written by run 6 | `desktop_audio_client` | 248 s FAIL | 14 s GREEN |

**The profile is a feedback loop, and it is bistable rather than a one-way
latch.** What run 2 recorded for `desktop_typing_damage` is 243 s of mostly
*waiting for the host*, and that number is what put it back beside
`desktop_window_child` in runs 3 and 4: a duration profile whose entries include
contention cannot order its way out of the contention it measured. It releases
the same way it engages — run 5 had no profile at all, measured 17 s, and run 6
read that and left the two apart. Run 7 then promoted the *other* desktop into
the same slot, which is what says the victim is positional and not a property of
any one test.

Deleting `target/test-durations` unsticks a worktree that is in the red state.
**That is a diagnosis, not a fix**: it does not stop the next run promoting
another desktop into the slot, and a green bought that way is a green about the
ordering rather than about the tree.

Consequences worth stating separately:

- **The four minutes buy a red nobody acts on.** `qemu::budget`'s width scaling
  is right for a liveness guard on a healthy test and wrong for one that is
  known to run out: `desktop_window_child` will spend the whole ceiling on every
  run until #156 closes.
- **Two of the three desktops' verdicts are typing windows**, which is what
  `Sched::Parallel` is not for. The bullet above says the budget was "evidently
  not enough of it"; this says what it is not enough *against*.
- **`desktop_audio_client` is not a second #156.** It fails with
  `desktop_typing_damage`'s message and `ALONE: GREEN`, which is this entry and
  not the freeze, so it does not belong in `EXPECTED_FAILURES`.

Not fixed here, and the fix is not obviously "reclassify": `Sched::Serial` for
all three desktops moves ~5 minutes into the serial tail, which
`specs/test-cost-audit.md` §5.4 spent a wave getting out of. Candidates worth
pricing: cap what a lane will spend on an `EXPECTED_FAILURES` test, exclude a
test's contention wait from what the profile records, or give the profile a
notion of how much host a task wants so two eight-CPU guests do not pair.
**Whichever it is, a landing is currently a coin toss** — three of seven runs in
one session were red on this and nothing else.

**Cost another landing on 2026-08-07**, task #133, with the same signature and a
wider margin than any row above: `desktop_audio_client` **787 s** in the wide
phase against **14 s** alone, on a host carrying four other worktrees. Its
verdict line was its own (`1 of the two overlapping clients left the mixer`)
rather than `desktop_typing_damage`'s, so the message is not the tell — the pair
of durations is.

**And the holder can be its own victim.** 2026-08-07, task #152's worktree,
whole suite 500 s against the ~109 s it is ordered for, with another worktree's
toolchain build on the host: `desktop_window_child` itself took **249 s** and
failed with `nothing typed at the terminal window reached a shell` — the
*victim's* message, from the test that is normally the one occupying the lane.
So the row above's "the victim is positional" is the weaker half of the claim:
under enough host pressure the position that loses its typing window is
whichever guest is late, and that can be the four-minute test as easily as the
one beside it. It also means **`EXPECTED_FAILURES` does not absorb it** — the
entry deliberately covers only "the desktop ceased to answer after a window
closed", and this message names a shell that never answered in the first place,
so the run reds on the very test the exemption exists for and for a reason the
exemption is right to exclude.

**And it is what is left after the TLB deadlock closed.** Four full suites on
`wt/toyos-tlbfix` on 2026-08-07, one before that fix and three after: the reds
were `metal_sim_compositor_stall`, `metal_sim_client_death`,
`screen_blocked_dump`, `i8042_mouse`, `desktop_audio_client` (385 s wide against
13 s alone) and `screen_blocked_dump` again — six of the seven `ALONE: GREEN`,
every one of them this entry. The one clean 289/289 run is the one whose suite
took **182.7 s**; the three red ones took 559, 576 and 705. That is the whole
correlation, and it says the remaining landing blocker is this section rather
than anything in the kernel.

**Two of those seven were not this entry**, and that is the caution the rest of
it now carries. `screen_blocked_dump` reds at the same ~20% with the host to
itself, and the defect was in the kernel (§5, closed 2026-08-08). `ALONE: GREEN`
on a test that is red one run in five says "this re-run was one of the four
green ones", not "the phase did it" — so the classification is evidence only
where the alone rate is known to be zero, and nothing measures that.

**Two of this entry's three named victims are closed as of 2026-08-08, and the
mechanism was not the scheduler.** Both `desktop_typing_damage` and
`desktop_audio_client` reached their shell through `shell_answers`, which
retyped `echo <nonce>` against `qemu::budget(20 s)` because nothing knew when
the terminal was up — so "how long does a desktop take to come up on the host of
the day" *was* the verdict. The terminal prints `terminal: ready` now (and
`/bin/console` already printed its own), so the coming-up half waits on the
guest's own liveness and only the keystroke round trip has a clock on it.
`close_focused_window` took the same guard and it cuts the other way there: #156
is a freeze, so the machine goes quiet and the wait ends in fifteen seconds
instead of spending up to four minutes at width 12 — which is the lane
`desktop_window_child` was holding for a quarter of every run, and the whole
mechanism of "whichever other desktop lands beside it loses its typing window".

`qemu::budget` also scales by how fast the host is now, not only by how many
guests are on it (`specs/ci-plan.md` §7.2). Neither change touches
`screen_blocked_dump`, `i8042_mouse`, `screen_console_scroll` or the rest of the
list above, whose verdicts are elsewhere — this closes the desktop family's
share of it and no more. Measured after: four desktop tests wide, 16/27/28/31 s
and 18/41/48/20 s in two runs, all green; a 291-test suite at width 12 in 478 s
with `desktop_window_child` no longer in the long tail.

### A whole parallel phase can be starved by another agent's build

Measured 2026-08-04: the same tree that runs the phase in 44.8 s ran it in
245.2 s with three other `cargo test` processes and a `toyos-sched-sim measure`
on the host. Nothing in the suite reports this — the phase is simply slow, and
whichever load-sensitive `Sched::Parallel` test loses its margin first is what
goes red. `uptime` before and after a suspicious run, and `ps aux -r | head`,
are what separate it from a regression; a `toyos-tests-<pid>` directory per live
suite in `$TMPDIR` names how many are up.

`buildlock::guest_slot` bounds the *guests* to twelve across all worktrees, and
that is the part of this the semaphore closes. `buildlock::build_slot` (added
2026-08-07) bounds the compiles to four, which closes the part this entry is
actually about — "another agent's build" is a `cargo test`'s or a
`cargo run`'s, and both go through `src/build.rs`. What is left outside both
counts is work that reaches neither: a `toyos-sched-sim measure`, a `cargo build`
typed by hand in a fork clone, `./x.py` run directly in `rust/`. Each wait is
announced (`[host-slots] waiting …`, `[host-builds] waiting …`), which is what
separates a slow phase from a starved one without `ps`.

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

### A fork pin is a moving branch frozen per workspace, and nothing re-reads it

`[patch]` names a *branch*; a lockfile records the rev that branch pointed at
when that workspace was last resolved. Six workspaces in the tree resolve fork
branches independently — the root, `toyos-cc/`, `userland/`,
`tests/toyos-rust-tests/`, `tests/toyos-rust-tests/tls-cranelift/`, and the
`rust/` submodule (which has two of its own). Push to a fork and every one of
them keeps the old rev until somebody happens to run `cargo update` in it. There
is no mechanism that notices, and no build that fails.

Measured 2026-08-07, before the fix in this branch: six pins were behind their
branch head, and two crates were pinned at *two different revs at once*.

- `libloading` — `fa0abe77` in `rust/Cargo.lock`, `2ca5f54b` in both
  `userland/Cargo.lock` and `tests/toyos-rust-tests/Cargo.lock`.
- `target-lexicon` — `45832ce6` in
  `rust/compiler/rustc_codegen_cranelift/Cargo.lock`, `50da81b3` in `Cargo.lock`,
  `toyos-cc/Cargo.lock` and `tests/toyos-rust-tests/tls-cranelift/Cargo.lock`.
  Two commits, and the one in the gap is semantic: `9aeabf5` adds
  `OperatingSystem::Toyos` to the SysV arm, so `Triple::default_calling_convention()`
  answered `Err(())` for ToyOS in `toyos-cc` and `SystemV` in cranelift. It was
  latent only because `CallConv::triple_default` folds `Err(())` into `SystemV`
  anyway (`cranelift-codegen-0.128.4/src/isa/call_conv.rs`) — a consumer that
  treated the error as an error would have diverged outright.
- `mio`, `socket2`, `tokio` — one `.gitignore` commit behind in
  `userland/Cargo.lock`; hygiene only.
- `raw-window-handle` — `76c4971c` where the branch head was `c39042b5`. This
  one had teeth: `forks.toml` claims the `toyos` branch is byte-identical to the
  head of PR #223, and that claim is about the *branch*, so the pinned tree was
  the pre-alignment one. The suite was validating code we had not sent and not
  validating code we had.

The `rust/` submodule's own eight pins were all at branch head, so this is a
monorepo-side drift, not an estate-wide one.

**The check that would catch it does not exist, and its shape is a decision for
the owner.** Compare each lockfile's `git+…#rev` against `git ls-remote <url>
<branch>` and fail on a mismatch. That catches all six. It also puts GitHub on
the path of whatever runs it, so it must not be a `cargo test` member or part of
the landing gate — an on-demand `cargo run -- --check-forks`, run when the estate
is touched, is the shape that costs nothing when the network is down. The purely
offline alternative — assert every lockfile agrees with every other about a
`(repo, branch)` pair — needs no network but is vacuous in a worktree, where
`rust/` is not checked out: it would have caught neither `libloading` nor
`target-lexicon` from where an agent actually works, and nothing at all for the
four crates that appear in exactly one lockfile. Not worth building.

### Two winit clones exist, and the canonical one is the stale one

`/Users/jan/Dev/jan/forks/winit` is at `be9ec72c`; `/Users/jan/Dev/jan/winit` is
at `faf99eb7`, which is `origin/toyos` and the rev every lockfile pins. Both are
clean and on `toyos`. `.cargo/config.toml.example` documents `../forks/<name>` as
the path-override convention, so `forks/winit` is the one an agent told to edit
"the winit fork" will find and path-override — and it is a commit behind, which a
path override silently substitutes for the pinned tree.

Outside the repo, so no commit fixes it: the owner should delete
`/Users/jan/Dev/jan/winit` (nothing is unpushed in it) or fast-forward
`forks/winit` and delete the stray. No other fork has a duplicate — checked
across all 13 clones under `forks/`.

### Every fork clone still carries its pre-rebase history, on no remote

`git rev-list <branch> --not --remotes` over the 13 clones finds **66 commits
reachable from no remote ref at all**, every one of them on a local `master` or
`main`: cpal 9, mio 11, socket2 8, libloading 8, stacker 6, getrandom 5,
target-lexicon 4, memmap2 4, ctrlc 3, tokio 3, russh 2, raw-window-handle 1,
softbuffer 1, winit 1. They are the original ToyOS work committed straight onto
each fork's `master` before the 2026-07-28 re-basing built the clean `toyos`
branches on pinned upstream bases — the commit titles are the tell (`Add brief
README for ToyOS fork orientation`, `Add .DS_Store to gitignore`, target-lexicon's
`hack: silence warnings`), and that cruft is exactly what `forks.toml`'s header
says was reverted.

**Nothing is lost, checked rather than assumed.** For every fork, `origin/toyos`
is identical to `master` on the ToyOS-specific paths or ahead of it: socket2,
mio, winit, softbuffer, stacker, libloading, ctrlc byte-identical; cpal's
`src/host/toyos/mod.rs` differs +108/-61 with `toyos` holding the newer futex
state machine and `PERIOD_FRAMES` that master's `AtomicBool`/`BUFFER_FRAMES`
predate; raw-window-handle's +44/-5 is the PR-alignment commit; memmap2's
`src/toyos.rs` exists only on `toyos`; getrandom's `toyos-0.2` carries it at the
0.2-era path `src/toyos.rs` rather than `src/backends/`.

So this is dead history, not work. But it is genuinely unpushed, which is the
honest answer to whether the estate is clean and pushed, and it is what makes
`git log --all` in any of those clones misleading. Deleting the local `master`
branches is the obvious close — outside the repo and the owner's call, and
explicitly not something an agent should do on its own, since a fork's history
is what an upstream PR is made of.

### A `toyos` branch mostly has no upstream, so `git status` cannot say if it is pushed

13 of the 16 consumed and PR branches across `forks/` have no tracking ref:
`git for-each-ref --format='%(refname:short)|%(upstream:short)' refs/heads` gives
`NO UPSTREAM` for cpal, ctrlc, getrandom (all three), mio, raw-window-handle,
socket2, softbuffer, stacker, target-lexicon, tokio and winit. Only libloading,
memmap2 and russh track `origin/toyos`, and target-lexicon's `add-toyos-os`
tracks `upstream/main` — which is why it reads `ahead 1` rather than in sync.

The consequence is that `git status` on a fork's `toyos` branch prints `## toyos`
and nothing else: the ordinary way to ask "have I pushed this?" is silently
unanswerable in the clones where the answer matters. Every one of them happened
to be in sync on 2026-08-07, established by comparing `rev-parse HEAD` against
`rev-parse origin/<branch>` rather than by reading `git status`. One
`git branch -u origin/<branch> <branch>` per clone fixes it; outside the repo,
so the owner's hands.

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

### CLOSED — `toyos-cc` gave a different object for the same source

Three `HashMap`/`HashSet` iterations in `codegen` were the emission order of
three different things, and `RandomState` reseeds each of them per map:

- `tentative_info` (`codegen/mod.rs`) — the order tentative definitions are
  zero-inited in, which is their order in BSS. This is the recorded symptom:
  `d_event.c`'s `eventhead`, `events` and `eventtail` trading offsets 0, 4 and
  0x500.
- `variadic_stubs` — the order the x86_64 variadic call stubs are emitted in.
- `FuncCtx::addr_taken` — the order the stack slots of address-taken parameters
  are cut in, so it moves code and not just data.

All three are now `BTreeMap`/`BTreeSet`, which is the whole fix: the container's
own type carries a total order, so an iteration that reaches the object writer
cannot be an unordered one. Nothing else in the crate iterates a hash container
into the output — `defined_data` is membership-tested only, and `strings` and
`tentative_data` were already `Vec`s.

Measured before and after on doomgeneric's 83 compilable sources, one release
binary: before, 42 of 83 objects differed between two runs; after, 0 of 83
differed across five runs. `toyos-cc/tests/determinism.rs` is the gate — three
cases naming one hazard each, compiled eight times per process, plus a sweep of
the 133 tinycc cases that compile, driven through the binary so the seeding is a
build's. It runs in 3.6 s with no guest. Negative control: with the fix
reverted, each of the three targeted cases went red 40 times out of 40 and the
corpus sweep named 6 files. Two runs per case rather than eight would have been
39/40 on the spilled-parameter one, which is why it is eight.

### CLOSED — `toyos-ld` gave a different binary for the same objects

The larger half of the same hole: the compiler's outputs are intermediate, and
the linker's are the kernel, the bootloader and every binary in the image the
owner flashes. Four hash containers were the emission order, one more than the
two the original write-up named, and the third is not a symbol table:

- `ElfLayout::{got, dyn_got}` — `reloc.rs` iterates them to build `relatives`
  and `glob_dats`, which are `.rela.dyn` and, through the import strings, the
  `.dynsym`/`.dynstr` that name them.
- `LinkState::{globals, locals}` — `emit_elf.rs` walks both to build the symbol
  entries, so this is `.symtab` and the `.strtab` that follows it. `emit_macho.rs`
  walks `globals` too, but sorts by name at the call site, which is why Mach-O
  never showed it.
- **`resolve_libs_with_entry`'s archive pull-in worklist** — seeded from a
  `HashSet` of undefined symbols and grown from `HashSet`s of each member's
  references. Where two archive members define one symbol, the member pulled in
  is whichever the worklist reaches while that symbol is still undefined, so a
  hash order decided **which sections existed at all**. Not a symbol-table
  defect and not reachable by sorting anything the writer sees.

All of them are `BTreeMap`/`BTreeSet` now, with `Ord` derived on `SectionIdx`,
`ObjIdx` and `SymbolRef`. That order is over an input position and a name, both
of which byte-identical inputs fix, so the derive removes the nondeterminism
rather than moving it. The rule the crate now follows, stated on `LinkState`: a
container iterated into the output carries its own total order, one asked only
whether it contains a name stays a hash container.

**PE did not share the defect** and passes the gate with the fix reverted:
`PeLayout::got` is iterated in exactly two places, one writing disjoint 8-byte
GOT slots and one pushing into `abs_fixups`, which is sorted before it is
written. PE also emits no symbol table. It is a `BTreeMap` now regardless,
because "safe by what the call site happens to do" is the rung below.

Measured. Real corpus, one release binary, eight links each: the toyos sysroot's
30 rlibs linked `--shared` (17.8 MB of objects, 1,855,584-byte output) differed
from run 0 in 7 of 7 later runs before, worst delta 26,408 bytes; 0 of 7 after.
The `x86_64-unknown-none` sysroot linked `--static` went 7 of 7 (40 bytes) to 0
of 7. The `x86_64-unknown-uefi` sysroot linked `--pe` was 0 of 7 both times.
Cost: the link phase of that shared link is 0.017 s before and 0.023 s after
(median of 15 interleaved runs), +6 ms for `BTreeMap` lookups on ~35 objects.

`toyos-ld/tests/determinism.rs` is the gate — eight cases naming one hazard
each, inputs synthesized with `object::write` so the test needs nothing outside
its own crate, each linked eight times through the binary because a build is a
process per output. 0.25 s, no guest. Negative control with the fix reverted:
each of the seven hazard cases red 40 times out of 40, PE green 40 out of 40.
Two runs per case rather than eight was also 40 out of 40 — these cases are
wide by construction; the eight is margin for a narrower one added later, which
is the case `toyos-cc`'s gate has.

### OPEN, UNASSIGNED — `toyos-ld`'s alloc-shim table names a compiler hash that no longer exists

`ALLOC_SHIMS` and `SHIM_NO_ALLOC_UNSTABLE` in `toyos-ld/src/collect.rs` are
eleven string literals of the form
`_RNvCs2fcwfXhWpkc_7___rustc12___rust_alloc`, where `Cs2fcwfXhWpkc` is a rustc
crate disambiguator. Measured 2026-08-07: that spelling occurs **zero** times
across the 30 rlibs of the `x86_64-unknown-toyos` sysroot, and the live
disambiguator is `CshVjSbrpHdcL` — 4 occurrences in `liballoc`, 5 in
`libpanic_abort`, and so on. So `synthesize_alloc_shims` currently synthesizes
nothing, and would again the next time `rust/` is rebuilt.

Inert today, because rustc emits the allocator shim into the leaf crate's own
object for a real binary; the table only matters for a link that has rlibs and
no leaf crate. Found while assembling a real corpus for the determinism gate,
which is exactly such a link and which failed on those six names.

The defect is the shape rather than the staleness: a compiler-internal hash
frozen into a string literal has no way to announce that it has gone stale, and
the symptom when it does is an undefined symbol far from the cause. Matching on
the `___rustc` path and the function name, with the disambiguator wild, would
not need updating.

### OPEN, UNASSIGNED — `toyos-cc`'s preprocessor exits the process instead of returning

Three `process::exit(1)` calls in `toyos-cc/src/preprocess/mod.rs`: a `#error`
directive (line 309) and a missing include, system or otherwise (lines 527,
530). A library denying its caller the choice — the compiler cannot report the
diagnostic in its own format, cannot continue to find a second error, and
cannot be embedded in a driver that wants to keep going. Every other error in
the crate returns. Recorded by the determinism task rather than fixed, on the
owner's standing rule about staying focused.

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

### OPEN — the initrd ships two git-ignored files, so the image is not reproducible from the repo

`system.toml`'s `assets = ["assets"]` sweeps the directory whole. Two of the
files it finds are not in VCS:

```
$ git check-ignore -v assets/.DS_Store assets/target/.deps-stamp
.gitignore:6:.DS_Store	assets/.DS_Store
.gitignore:1:target/	assets/target/.deps-stamp
```

and both reach the image. From the 2026-08-07 build:

```
initrd: adding 'share/.ds_store' (6148 bytes)
initrd: adding 'share/target/.deps-stamp' (10220 bytes)
```

A fresh clone therefore builds a *different* initrd from this checkout's, and
`.DS_Store` changes whenever Finder touches the folder — so the image hash moves
with no code change at all. That is the same property `toyos-cc` and `toyos-ld`
were made deterministic for on 2026-08-07, defeated one directory up.

`assets/target/` is stray: an empty `target/` with one `.deps-stamp`, mtime
2026-08-01 08:28, the same minute as the `.DS_Store` — something ran cargo with
`assets` as its working directory. It should be deleted; the sweep should refuse
anything git ignores, or take an explicit list.

Costs 16 KiB of a 672 MB initrd, so nothing breaks. The reproducibility claim
does.

### OPEN — every toolchain build runs Python, and every host link runs `cc`

`specs/dependency-audit-2026-08-08.md` §3–§4 is the full inventory; this is the
entry that says the two largest holes in *"Rust and QEMU, one command"* are real.

`src/toolchain.rs:749` picks `./x` when `rust/x` exists, which it does. That file
is a `/bin/sh` script whose whole job is `SEARCH="python3 python py python2 uv"`,
and it execs `x.py` → `src/bootstrap/bootstrap.py` (55,550 bytes). So a clean
clone cannot build a toolchain without Python 3. It is upstream's bootstrap and
not our code, which is why it is stated rather than blamed — but the bar has no
upstream exemption, and `bootstrap.py` can never run inside ToyOS.

Separately, and measured with `rustup run toyos rustc --print link-args` on a
trivial host binary: rustc invokes `"cc"` and sets
`SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk`. rustup installs
neither. Every *host* binary goes through it — the build system, the harness,
`toyos-ld`, `toyos-cc`, rustc stage2. **No guest binary does**: both
`.cargo/config.toml`s under `bootloader/` and `kernel/` set
`linker = "toyos-ld"`, so nothing that boots is touched.

`src/main.rs:7`'s preflight checks `git`, `rustup` and `qemu-system-x86_64` and
says nothing about either of these. The cheap half of the fix is to make the
preflight and the README say what the machine actually needs.

### OPEN — `df`, `ps` and `find` are external binaries the bar does not allow

Three more, none of which comes with Rust or QEMU. Audit §5.

- `src/worktree.rs:141` — `df -k`, `.expect("run df")`, so `worktree add` dies
  without it. One free-space number.
- `tests/common/hostload.rs:111` — `ps -Ao comm=` for gate A's `host:` line.
  Degrades correctly (`.ok()?`), so its absence costs a diagnostic and not a run.
  Its neighbour `getloadavg` is already reached through the `libc` crate, which
  is the shape this one wants.
- `toyos-fat32/tests/common/mod.rs:301` — `find <mount> -name '._*' -delete`,
  sweeping macOS resource forks off a freshly-populated volume. **Not on the
  known list of that file's macOS tools**, and it belongs there: it is reached by
  the same fixtures as `newfs_msdos`/`hdiutil`/`fsck_msdos`, which is all 59
  `#[test]`s in `toyos-fat32/tests/`.

Reach of the three FAT tools, since "nine tests" understates it:
`fsck_complaints` is reached by five `MACHINE_TESTS` entries (`esp_filesystem`,
`toybox_cp_volume`, `kernel_log_file`, `log_partition_layout`,
`log_partition_identity`), `src/image.rs`'s own `fsck` by two `#[test]`s that run
under `cargo test --lib` — which is in the landing gate — and
`toyos-fat32/tests/common/mod.rs` by all 59.

### OPEN — `/bin/doom` is built from a moving branch head nobody pinned

`userland/doom/build.rs:download_doomgeneric` fetches
`https://github.com/ozkl/doomgeneric/archive/refs/heads/master.tar.gz`. No
commit, no tag, no hash check. It runs once, when `userland/doom/doomgeneric`
does not exist — so **which Doom sources you build is a function of when you
first cloned**, two developers can differ, and nothing reports it.

doomgeneric is a third-party C codebase this project builds and ships. CLAUDE.md
says the sanctioned form is a fork: a real repo, a pinned base, a `toyos` branch,
an entry in `forks.toml`. It has none of those and is not in `forks.toml` at all.
This is the same reproducibility property `toyos-cc/tests/determinism.rs` and
`toyos-ld/tests/determinism.rs` exist to protect, lost one layer above them.

Five build-dependencies exist only to serve that download and the SoundFont one
beside it — `ureq`, `webpki-roots`, `flate2`, `tar`, and `rustls-rustcrypto`
pinned at `"0.0.2-alpha"`, which is hard to square with *"only general and often
used rust crates"*. Making doomgeneric an ordinary fork takes all five with it.

**And the downloads race the suite**, measured 2026-08-08 on this branch's first
landing gate. A fresh worktree has no `assets/timgm6mb.sf2` — it is gitignored —
so `metal_sim_compositor`, `metal_sim_pointer_churn` and `boot_partition_identity`
red with *"declared in `untracked-assets` and is not there"*, while by the end of
the same run the file was on disk with an mtime inside it and all three passed
alone. Nothing sequences `download_soundfont` against the initrd builders that
want its output, and the failure reads as a missing asset rather than as a race.

### OPEN — `dosfstools`, which was refused, is installed by a committed workflow

`.github/workflows/probe-toolchain.yml:39` installs it beside `qemu-system-x86`
and `ovmf`. It only runs on `ci/probe-toolchain` pushes, so it is not on any
path `main` takes — but it is committed, and a refused dependency left in the
tree is how it comes back. `specs/ci-plan.md:242,404` discusses `fsck.vfat` as an
option and does not record that the answer was no.

### OPEN — what the repository ships is not all MIT OR Apache-2.0

`specs/dependency-audit-2026-08-08.md` §7 has each item with its evidence. Six
committed binary files — 11,094,369 bytes of the 14,634,080 git tracks — plus one
committed source corpus sit outside the declared licence, and the README declares
only one exception
(`userland/doom` is GPL-2.0). Provenance had to be established from content,
because `assets/` and `ovmf/` both entered in the initial squashed commit
`52eb78e` and carry no attribution.

Sharpest first:

- **`tests/testcases/tinycc/46_grep.c`** carries `Copyright (C) 1980, DECUS` and
  *"General permission to copy or modify, **but not for profit**"*. A non-free
  clause, in a repository offered under MIT. It sits in TinyCC's `tests2` corpus
  — 314 files here, 51 more in `tests/testcases/pp_tcc/` — which is LGPL-2.1 and
  carries no `LICENSE`, `COPYING` or `README` of any kind.
- **`assets/DOOM1.WAD`** (4,196,020 bytes, `IWAD`, 1,264 lumps) is the id
  Software shareware IWAD, redistributable only on its own terms. It ships in
  every image. The README's GPL note is about doom's *code* and does not cover it.
- **`assets/JetBrainsMono-Regular.ttf`** says in its own name table that it is
  SIL OFL 1.1. No OFL text is in the repository. Two derived artifacts inherit
  the question: the committed `kernel/src/drivers/panic_console/font8x16.bin` and
  the `share/fonts/*.font` tables in every initrd.
- **`assets/icons/*.svg`** are all eight byte-identical to Phosphor Icons files
  (four from `SVGs/bold/`, four from `SVGs Flat/bold/`; verified with `cmp`).
  Phosphor is MIT and requires its notice; the `phosphor-icons/` directory
  holding that notice is `.gitignore` line 7.
- **`ovmf/*.fd`** (6,291,456 bytes) are a third-party EDK II build —
  `edk2-gf0064ac3af`, off a Jenkins worker, read out of the binary — with no
  licence, version or recipe recorded. They are load-bearing: `src/qemu.rs:101`
  and `tests/common/qemu.rs:2090` point pflash at them, while every workflow
  installs the `ovmf` apt package and the harness ignores it.
- **`assets/wallpaper.jpg`** has **no determinable provenance**. Its EXIF holds
  four tags and neither `Artist` nor `Copyright`; git history has nothing; the
  README does not mention it. It ships as `share/wallpaper.rgb`. Recording the
  open question rather than guessing is the point of this bullet.

### OPEN — nothing in the tree checks any of the above

Both violations that prompted the audit — `fsck_msdos` and the SoundFont's GPLv2
— existed for months and were found by collision. There is no ledger of allowed
crates, allowed binaries, or asset provenance, and no check reads one.

`specs/dependency-audit-2026-08-08.md` §11 proposes three, all offline, all
inside `cargo test --lib`, and prices each including what it cannot catch — the
crate ledger is the one with teeth, the binary-literal scan is the weakest, and
neither reaches `rust/`'s own dependencies or a third-party build script. The
same constraint that governs fork pins governs these: **anything touching the
network must be an on-demand command, never `cargo test` and never the landing
gate.** Nothing was built, deliberately: every one of these would go red on the
tree as it stands, and seeding the ledgers is a decision about which findings
above are accepted.

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
the ELF loader (`elf/mod.rs`'s `load_shared_lib`, size and allocation share
`load_size` by proximity, not by construction — and every past OOB in the loader
came through this type), and `DmaPool::alloc` (`drivers/mod.rs:34`).

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

### Metal boot is 1151 ms against QEMU's 196 ms, and the recorded accounting for it is stale

**The numbers, taken out of the committed logs rather than re-measured.** Six
healthy boots in `specs/metal-logs/2026-08-07-freeze/` report `Boot: complete` at
1148, 1148, 1149, 1150, 1151 and 1154 ms; the seventh (`…-222741.log`) is 755 ms
and is the control boot whose keyboard was refused, so its peripherals phase is
448 ms instead of 842. The QEMU figure for the comparable shape is
`Boot: complete (196ms)` on the `metal_sim_compositor` boot (§8's i8042 entry
below records the measurement), and `(234ms)` for the diag artifact booted
headless. **So metal is ~5.9× QEMU, not the ~17× `specs/metal-hardware-inventory.md:392-395`
computes** — that ratio is against `(3422ms)`, and 2.30 s of those 3.42 s were
the six `boot_checkpoint` framebuffer repaints (`metal-hardware-inventory.md:425-429`),
which #138's write-combining change removed. Measuring the phase-boundary gaps in
`…-223244.log` myself, all six together are **73 ms** against that boot's 2308.
The inventory's "Boot timing on metal" section describes a machine that no longer
exists and should be re-taken or dated.

**Where the 1151 ms goes now**, from `…-223244.log`:

| phase | reported |
|---|---|
| CPU ready | 60 ms |
| storage ready | 84 ms |
| **peripherals ready** | **842 ms** |
| subsystems ready | 93 ms |
| devices ready | 20 ms |

Peripherals is 73% of the boot, and its two largest components are:

- **393 ms of i8042 keyboard init** — `i8042: ok selftest=0x55` at 0.609, the
  next i8042 line at 1.002. Real hardware, not a probe of an absent one.
- **206 ms establishing the Thunderbolt xHC at `00:0d.0` has nothing on any
  port** — `controller started` at 0.161, `no HID devices on the controller at
  00:0d.0` at 0.367. Four of the PCH's port resets are 55 ms each, which is USB's
  own and not the driver's to shorten.

**Absent-device probing is ~279 ms of 1151, not ~1.1 s.** The other piece is the
PCI walk: `PCI: Enumerating devices...` at 0.065, last real function `0a:00.0` at
0.072, `Enumeration complete, 24 functions.` at 0.145 — **73 ms** scanning buses
that hold nothing, against 7 ms finding everything that is there.

**What this entry is for.** Metal boot time has no heading of its own; the
accounting lives in `specs/metal-hardware-inventory.md` against the superseded
3422 ms boot, and `known-issues.md`'s console entry below points at "#65 (boot
time)" as its owner. Whatever #65 says, its numbers should come from this table:
the two-thirds that motivated it were paints and are gone. Note also the NIC
retry that looks like boot cost and is not — `toyos/src/net.rs:271`'s 100 retries
at 10 ms run *after* `Boot: complete` (see *Every network client pays a second of
boot retry on a machine with no NIC*), and `READY_BUDGET_NS` bounds retries
rather than boot time (§9).

### One atomic read-modify-write per log line cost 350 ms of boot

Measured 2026-08-08, interleaved A/B in one session, `xhci_slow_connect`'s own
boot line as the instrument:

| kernel | `Boot: complete` |
| --- | --- |
| `main` | 497, 497, 498, 501, 503, 504 ms |
| `main` + one `WRITTEN.fetch_add(n, Release)` in `log_ring::append` | 812, 816, 817, 826, 832, 839 ms |
| the same, as a load + store under the lock it already holds | 498, 500, 500 ms |

The `fetch_add` is **outside** the byte loop — once per `write_chunk`, a few
hundred times in a boot. It is a single `lock xadd` on an uncontended line, and
on real hardware it is tens of nanoseconds. Under TCG it is not one instruction:
QEMU cannot always emit an inline host atomic for a guest RMW and falls back to
leaving the translation block to run it exclusively, which is hundreds of
microseconds each. A few hundred of those is a third of a second.

**Why it is worth an entry rather than a comment.** The first A/B said the
regression was 200 ms, the second said 350, and a *third* build — the same
source with a timing `log!` added to `boot_checkpoint` — measured 500 ms and no
regression at all, because the extra call changed inlining enough to move the
cost somewhere the instrument could not see it. So an instrumented build
disproved the defect that the uninstrumented one reproduced 5 times out of 5.
Bisect the **source** when that happens, and interleave the arms: the first
uncontrolled A/B here ran all of one arm and then all of the other, and the
host settled in between, which made a reproducible 350 ms regression look like
host noise.

Nothing else in `log_ring`'s hot path does an RMW — `OWED`, `FILE_OWED` and the
cursors are all plain stores under `RingGuard`, and the comment there now says
why. `DROPPED_BYTES` and `FILE_DROPPED` are `fetch_add`s, but only on the
overflow path.

### `xhci_slow_connect` has a 1 ms margin, and it is what caught the above

`SLOW_CONNECT_NS` holds the ports empty for 0.3 s and the controller starts at
**0.296–0.311 s** on a quiet host, so the gate reds whenever anything moves boot
by ten milliseconds. That sensitivity is the reason the log-ring regression was
caught at all — no other gate in the suite noticed 350 ms — and it is also why
the test reds on a loaded host for no reason of its own. Its own message names
the fix (`widen SLOW_CONNECT_NS, not this gate`) and it belongs to whoever owns
`toyos_xhci`; recorded here from a landing gate, not fixed.

Distinct from §7's parallel-red class: that one is about *verdicts* that are
wall-clock margins on the **host** side, and re-running alone clears them. This
margin is inside the **guest's** boot, and running alone only moves it back
under the line by a few milliseconds.

### OPEN — six input tests fail on a GitHub runner and pass here, and nobody has separated the accelerator from the QEMU version

Found 2026-08-08 taking the guest suite to CI. Run `31238056513`, six
`ubuntu-24.04` shards on KVM at `--jobs 2`, 246 of 268 passing on the five that
finished. After the `ALONE: GREEN` contention class is set aside
(`specs/ci-plan.md` §7.2), the largest thing left is one class and every member
of it injects something:

| test | on a runner, **alone** | here, `--jobs 1` |
|---|---|---|
| `desktop_typing_damage` | 6 of the 16 echoes, 89 s | green, 41 s |
| `i8042_mouse` | stalled with `1007 of the 1007 packets injected came back out`, 82 s | green |
| `xhci_hotplug` | timed out, 86 s | green, 6 s |
| `metal_sim_pointer_churn` | `the churn did not reach the kernel`, 22 s | green, 17–37 s |
| `desktop_window_child` | `nothing typed at the terminal window reached a shell`, 303 s | green, 28–48 s |
| `xhci_flap` | timed out, 83 s | green, 6 s |

The eight suspects were run on the dev host at `--jobs 1` the same day and all
eight passed. So this is not the shape of a margin: `i8042_mouse` says every
packet it injected came back and it still stalled, and `metal_sim_pointer_churn`
says nothing reached the kernel at all.

**Two things differ and neither has been isolated.** The accelerator — these
guests execute on KVM and the dev host's emulate — and **QEMU 8.2.2 on
`ubuntu-24.04` against 11.0.3 here**, three major versions. Injection is a QMP
path through the emulated i8042 and xHCI, which is exactly the surface a version
gap would move; `-nodefaults` and the tablet/mouse device set are already known
to have shifted in QEMU 11 (`tests/common/qemu.rs` carries that comment).

Separating them is one probe workflow and two jobs: the same test on one runner
with `/dev/kvm` unlocked and one without. That is the §7 method and it settled
the SYSRET class in a single run. **Do not guess which it is** — if it is the
accelerator it is a guest defect and the owner's rule says so out loud; if it is
the QEMU version it is a harness assumption that this tree has never had a
second data point on.

### `i8042_mouse`: closed — the host outran QEMU's PS/2 queue, twice over

Both red modes are fixed and both were the harness. Neither was ever a packet
the driver lost.

**The count.** `MOUSE_LEAD` let the host hold 32 packets — 96 bytes — injected
but unreported, and justified it against "a 256-byte ring in the kernel and
QEMU's PS/2 buffer above it". That 256 is `PS2_BUFFER_SIZE`, the migration
array; the enforced capacity is `PS2_QUEUE_SIZE`, **16 bytes**
(`hw/input/ps2.c`, checked against QEMU v11.0.0, the version this host runs).
`ps2_mouse_send_packet` emits only while `PS2_QUEUE_SIZE - count >= 3` and
returns 0 otherwise, and `ps2_mouse_event` keeps accumulating `mouse_dx` while
it does — so a host past the queue does not lose a packet, it makes QEMU **sum**
several into one. The burst alternates +1/-1, so a summed pair is a packet with
`dx == 0`, and `mouse::handle_motion` queues nothing for a report that moves the
pointer nowhere with no button change. **Two injected, none delivered** — which
is why every observed shortfall was even (996/1004, 1002/1004) and why the
stalls sat at a deficit of exactly 32: losses accumulate until the lead is full
and the host never injects again.

Reproduced by pipelining QMP commands without awaiting their replies, which is
what an oversubscribed host does to the vCPU thread by accident. Floods of
4/8/16/32/64/128/256 back-to-back packets, two sweeps in one boot:

    [4, 8, 16, 32, 62, 128, 256, 4, 8, 16, 32, 64, 126, 256]
    [4, 8, 16, 32, 62, 128, 256, 4, 8, 16, 32, 64, 100, 256]

and in an earlier boot a 32 that delivered 18. Paced injection on a quiet host
never merged at any lead, including no pacing at all — which is exactly why this
only ever reddened under contention, and why a branch's kernel had nothing to do
with it. The last sighting before the fix was a landing gate on a branch whose
only diff was a crate nothing compiles and two documents: a byte-identical
kernel, red when re-run **alone**.

The fix is `MOUSE_LEAD = 4`, with the device's queue named in code and a `const`
assert that `MOUSE_PACKET * MOUSE_LEAD <= QEMU_PS2_QUEUE`. Raising it to 6 stops
the harness compiling, with that sentence as the message. The premise it rests
on — that QEMU sums motion between syncs rather than dropping it — is staged in
the run itself: `MERGE_MOTIONS` moves in one `input-send-event` must come back
as one packet of that many steps. Making `mouse_merged` send one command per
move instead reds it (1012 events for 1009 packets), so the stage has teeth.
Cost: the injection takes about 2× the guest time it did (578 ms → 1313 ms
measured alone), against a 60 s failure mode removed.

**The lost edge.** `service` reads the source's `irq_ring` record, then reads
the byte ring. The ISR fills the byte ring *before* it publishes its record, so
an interrupt landing between those two reads leaves the pass holding bytes it
has been told nothing about — and it counts a lost edge that never happened. The
record it left standing is taken by the next pass, which finds nothing to drain,
so nothing ever corrects the count. `service` now asks again once the bytes are
in hand.

Unwidened the window is a handful of instructions on one CPU, so nothing outside
the kernel reaches it — hence the `i8042-edge-race` actuator, which holds the
pass between the two reads. With it on and the fix reverted the counter reports
**116 and 127** false lost edges on one run of `i8042_mouse` and the test reds;
with the fix, 0 across every i8042 test. It is bundled into `i8042-trace`, so
the group that reads those counters is the group that runs it.

Both `Sched::Parallel` classifications stand: neither verdict is a rate, now for
a reason that is checked rather than argued.

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

`device::try_claim` gates `DeviceType::{Framebuffer, Nic, Audio}`
on an info struct the driver registered, so those three return
`ClaimError::Absent` when the hardware is absent — which is what makes soundd
and netd able to exit cleanly.
`DeviceType::Keyboard` and `DeviceType::Mouse` are gated on nothing at all: they
hand out a `Descriptor` whether or not any driver will ever produce an event. Under
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

### OPEN — pulling the boot stick freezes the T14, and the diagnosis that was wrong

**The report.** Pulling the USB stick while the desktop is up freezes the whole
machine unrecoverably, from a USB-A or a USB-C port alike, and **Ctrl+Alt+D does
not answer afterwards**. That last clause is the strongest signal available: the
blocked-task dump is dispatched from `drain_irqs` at the top of a scheduler
pass, so no CPU is reaching a pass — not three of eight as in the wedge, all of
them.

**A diagnosis to withdraw, recorded because it read well.** The mechanism first
proposed was: every CPU entering a pass takes `XHCI`, one holds it across a full
blocking teardown with 2 s waits against a device that cannot answer, and the
rest spin. **It does not survive its own prediction.** `sync::Lock::lock` logs
`LOCK CONTENTION: {N}M spins` at 50M and panics `DEADLOCK` at 500M
(`kernel/src/sync.rs`); a `pause` iteration is tens of cycles, so a CPU waiting
behind one 2 s hold passes the warning and one behind two approaches the panic.
The owner reports a freeze with neither a contention line nor a panic screen. So
"every CPU spins on the ticket for seconds" is not what happened.

What the code still supports, and all it supports: **one CPU holding `XHCI` for
the transfer budget per SCSI command against a device that has gone.** Whether
anything else was spinning behind it is not settled by any evidence in hand.

**The residual that makes this hard, as a category.** The evidence channel is
the thing that fails. `/log` is on the stick being pulled, so the event that
would be diagnosed destroys its own record — a contention line goes into a ring
drained to a file on a device that is no longer there, and the T14 has no serial
port. **A defect whose evidence channel is the failing component cannot be
investigated by reading the log afterwards**, and this will not be the last one:
any device carrying `/log` has the same shape. What would break it is a channel
that does not depend on the storage stack — the on-screen panic console covers a
panic, and this is not one.

**What `c4ba7d5` closes.** The amplifier every candidate path shares.
`wait_transfer` ended on the clock; it now ends on the register when the slot's
port reads disconnected, because a device that has been unplugged is not a
device that is slow. A filesystem sync, a page-cache fill, a teardown and a
scheduler pass all reach that function, and pulling the stick a machine logs to
aims all of them at a dead device on one event.

**What it does not close, stated so a green suite does not imply otherwise:**

- ~~Teardown and `recover_endpoints` still block a pass~~ — **closed by X2a**
  (`specs/xhci-port-machine-plan.md`). Both are submit-and-return against one
  outstanding operation per controller: the pass that starts one gives itself
  back, and the completion arrives through the event ring the poll already
  drains. What is left on that path is `device::configure`, which is X2b; the
  type split that would make a wait there a compile error belongs with it,
  because a view that still has to hand `poll` a route to `configure` is a
  signature promising a check it does not perform. Two costs moved rather than
  going away, and neither is a defect: `PORT_WORK_AT` carries the outstanding
  operation's deadline, so an idle CPU declines to halt across a teardown
  exactly as it already does across a debounce, and a teardown now takes one
  further scheduler pass.
- **The metal claim is still the owner's to make.** Everything above is the
  guest-side proxy — no pass blocks — and the acceptance test is a stick pulled
  out of a running T14 with Ctrl+Alt+D still answering.
- `log_file`'s flush still holds `SINK` and the VFS across device I/O. The doc's
  "unbounded and uninterruptible" is half right, and the precise reading is
  **bounded in acquisition, unbounded in work**: `poll` is `try_lock` on both and
  disables the sink after `MAX_BLOCKED_NANOS`, so it never waits for a lock — but
  `Sink::flush` then calls `vfs.flush_file` and `vfs.sync_mount`, which reach
  `msc_write`/`msc_flush`, which take `XHCI` and spend the transfer budget per
  command.
- ~~There is no gate for the dangerous window~~ — there is now a gate for the
  *pull*, `usb_boot_stick_pulled`, and what it certifies is below. It is still
  not a gate for the 100 ms debounce and still cannot be aimed inside it.

**A negative result worth keeping.** The change did not make
`desktop_window_child` green; it stayed red across two landing gates. That is
evidence *against* the desktop freeze and the unplug freeze sharing the xHCI
path, and it agrees with the scheduler track's independent exclusion of the
ticket lock — two tracks reaching the same exclusion from different directions.

#### The instrument, and what it showed

`usb_boot_stick_pulled` (`tests/common/usb.rs`) is the first reproduction
attempt anywhere but the owner's desk. The boot stick had no QEMU device id —
every earlier unplug test names a *data* disk, which carries neither `/boot` nor
`/log` — so `qemu::BOOT_STICK_ID` is new and the pull is `device_del` on it. It
boots metalcase on `Profile::Metal` at eight cores with `log-rotate-fast`, so the
sink creates, sweeps, deletes and syncs rather than only appending; it drives a
drumbeat of `run` lines through test-runner, each a userland `println!` into the
ring the sink drains to the stick and a VFS walk for a binary that is not there;
it pulls the stick mid-drumbeat; and then it plugs one back in. The verdicts are
two liveness ceilings on paths that share no mechanism — the compositor's 2 s
frame report, which is the owner's clock that stops advancing, and a console
round trip that comes back through the VFS. A red prints `freeze_report`:
`info registers -a` first, Ctrl+Alt+D second.

**It is green, and the hazard was staged rather than missed.** The pull landed
with a write outstanding — `transport broke on SCSI 0x2a: no answer in the
command phase`, then `slot 1 would not take a Bulk-Only Reset`, then
`reset recovery failed; disk is offline`, all 89 ms *before* `xHCI: port 1
disconnected` — which is the device-gone-teardown-not-yet-run window the entry
above says cannot be aimed at. It was hit by accident and survived: 40/40 console
probes answered and the desktop kept drawing, and 40/40 again after the replug.

So the finding is that **QEMU does not reproduce it**, and the reason is
visible: `c4ba7d5` ends a transfer when the slot's port reads disconnected, and
`device_del` drops CCS at once, so nothing here ever spends the 2 s budget. What
follows is therefore read out of the code rather than watched.

#### One hypothesis killed by the code, on the machine that matters

**`drain_serial`'s `BackendGuard::lock` cannot be the T14's freeze.** That
machine has no 16550 and no virtio-console, so `serial::has_console()` is false
for the whole boot and `log_ring::set_serial_sink(false)` pins `LogRing::len` at
zero. `drain_chunk_to_serial` therefore returns 0 on its first call,
`drain_serial` leaves its loop, and the backend guard is held for one memcpy of
nothing. What makes that lock dangerous — unbounded work with interrupts off —
needs a backend, and that machine has none.

#### The mechanism the code does support, end to end

A chain, each link with the line that carries it. Nothing here has been watched;
every link is checkable by reading.

1. **One `XHCI` ticket lock serves every controller**
   (`drivers/xhci/mod.rs`, `static XHCI: Lock<Vec<XhciController>>`), and
   `storage_write`/`storage_flush` take it from whatever thread is writing — the
   idle loop's `log_file::poll`, a page-cache fill, a syscall.
2. **`poll_if_pending` takes the same lock at the top of every scheduler pass.**
   X2a made the *work* behind it submit-and-return; the *acquisition* is still a
   blocking spin against whoever holds it. A stick that has gone leaves port work
   pending, so every CPU entering `drain_irqs` reaches that `XHCI.lock()`.
3. **`Lock::lock` spins with interrupts enabled.** `preempt::disable()` is a
   per-CPU counter and touches nothing in `RFLAGS`. A convoy on this lock
   therefore reads `HLT=0` with `IF` **set** — neither of the two answers the
   #156 capture taught us to look for.
4. **At 500M spins it panics `DEADLOCK`.** On a CPU running a userland thread
   inside a syscall, `main.rs`'s `#[panic_handler]` takes the branch
   `percpu::syscall_rip() != 0 && percpu::current_tid().is_some()`, which calls
   `discard_capture()` and then `try_recover_from_panic()`. **A recovering panic
   never paints**, and on a machine with no console `panic_flush` has already
   returned early on `!has_console()`. The report exists in the ring and is
   discarded by the code that decided the panic was survivable.
5. **And the ticket is stranded.** `Lock::lock` does `ticket.fetch_add(1)` before
   it spins, and `now` is advanced only by `LockGuard::drop` — a thread that
   panics inside the spin never constructs a guard. Poisoning it leaves `now`
   permanently one short of every ticket behind it, so **every later acquirer of
   that lock waits forever, machine-wide, for the rest of the boot.**

The chain ends where the owner's machine does: nothing scheduled because no CPU
completes a pass, the clock stopped, the keyboard dead because a scancode is
decoded by `i8042::service` inside `drain_irqs`, Ctrl+Alt+D silent because
`keyboard::take_dump_request` is read from the same place, no panic screen, and
no log anywhere.

**This also repairs the argument that withdrew the ticket-lock diagnosis.** That
withdrawal rested on the owner seeing no `LOCK CONTENTION` line. On the T14 that
line is unobservable by construction — it goes to the log ring, whose serial sink
does not exist and whose file sink is on the stick that was just pulled — and the
absence of a line that cannot be printed is not evidence. What the withdrawal got
right is the missing *panic screen*, and step 4 accounts for that too.

**Two defects, either worth fixing on its own — and neither is this bug.** The
metal result below eliminates the chain as *the* mechanism; these stand on their
own reading and stay open.

- **A deadlock panic is classified recoverable because a syscall happens to be in
  progress.** The predicate asks whether a userland thread is current, not
  whether the kernel can continue. For `Lock::lock`'s own deadlock it
  demonstrably cannot, and the handler's response is to throw away the on-screen
  report and make the deadlock permanent.
- **A ticket lock cannot survive an abandoned waiter.** `sync.rs`'s
  `ticket.fetch_add(1)` happens before the spin and `now` advances only in
  `LockGuard::drop`, so a waiter that panics inside the spin never constructs a
  guard and its ticket is never served: the lock is unacquirable for the rest of
  the boot, machine-wide, with no diagnostic at all. The queue form of the
  "locks a dead thread can strand" class in §2, and worse than the held-guard
  form — the abandoning thread never held the lock, so nothing in the code reads
  as if it owned anything to release. Whatever closes it should make the failure
  loud and terminal rather than silent and permanent, and note that a fix whose
  only signal is a log line is invisible on the machine that needs it.

**And one removed here.** `poll_if_pending` took `XHCI` with `lock()` at the top
of every scheduler pass on every CPU, and while a port has work outstanding
every CPU finds it due — so the acquisition alone put as many CPUs as the
machine has on one ticket queue, each spinning with preemption disabled. It is a
`try_lock` now: the CPU holding that lock is doing precisely the work the
declining CPU came to do, so waiting for it buys a second look at a state
somebody else has already advanced. The `irq_ring` record is consumed only after
the lock is held, because `take` clears a slot an ISR coalesces into and a CPU
that took its record and then declined would have dropped a wake.

#### 2026-08-07: the metal result, and what it eliminates

The owner built at `8cfb6d8` — a clean tree carrying **X2a and X2b both** — and
pulled the stick. **Froze. Ctrl+Alt+D nothing. He then sat untouched for a full
minute and the panel did not change at all**; the desktop image stayed as it was.

**X2a and X2b are eliminated as the fix and are not eliminated as correct work.**
They removed real unbounded waits from the scheduler pass, their gates stand, and
the freeze is unchanged across them. What that buys is a clean narrowing: the
remaining xHCI candidate is the **acquisition** of the one controller lock rather
than the work done under it.

**The minute of nothing is the interesting half, and the arithmetic behind it.**
500M spins is seconds, not minutes, so a convoy would have reached
`Lock::lock`'s DEADLOCK panic dozens of times over. `main.rs`'s recovery branch
is per-CPU and conditional — `syscall_rip() != 0 && current_tid().is_some()` —
and an idle CPU satisfies neither clause, so it falls through to
`halt_all_cpus`, which paints. On eight cores running three processes, several
CPUs are idle. Something should have appeared. Two of the three explanations are
now closed:

- **"The panic console cannot take the screen from a compositor."** False, and
  now gated. `SCREEN_OWNED_BY_USERLAND` stops *boot checkpoints* and nothing
  else; GOP's `set_resolution` always returns `NotSupported`, so
  `gpu::set_resolution`'s missing `rearm()` on the success path never fires on
  this machine; and `panic_console::disable()` is virtio-gpu's alone.
  `screen_fatal_halt_composited` now boots metalcase with a compositor holding
  the panel and asserts the fatal report lands on it.
- **"A fatal panic on an idle CPU reports itself."** It did not, and that is
  fixed in this branch. `idle_loop` is entered by `jmp`, so its frame is the
  topmost on the 16 KiB idle stack and `rbp + 8` — which `kernel_backtrace`
  reads before checking it — was the unmapped page above. Every fatal panic on
  an idle CPU faulted inside `crash_report` while printing its own backtrace,
  the fault's report faulted the same way, and the chain ended in a double fault
  and `PANIC REENTRY`. Measured: **seven pages of cascade with no line saying
  what panicked, against one panic and three pages afterwards.**

So the "nothing paints" clause in the chain above survives, with a different and
checkable cause than the one it was written with — not the recovery branch, but
a report that could not be printed from the stack it was raised on. **What that
does not establish is that a deadlock panic is what happened**; it establishes
only that if one had, the owner would have seen a fault cascade or nothing, and
never the reason.

**A latent defect found on the way**, not live and worth an entry: on the
success path `gpu::set_resolution` calls `panic_console::detach()` and never
`rearm()`, so any driver whose resolution change *succeeds* blinds the panic
console for the rest of the boot. Unreachable today — GOP always refuses and
virtio-gpu is disabled outright — and one new GPU driver away from being live.

#### 2026-08-07, second metal round: the probe painted, and the convoy is retired

**The probe fired and painted on the T14**, over a compositor that was actively
drawing — the two lines above it on the panel are `compositor: frames=3` and
`frames=2`. Two backtrace frames, chain terminated, no cascade, no
`PANIC REENTRY`. The seven-page cascade is gone on the machine the fix was
written for, and the fatal path is now *proven* to reach that panel.

**That turns the minute of silence into positive evidence, and it retires the
chain above.** A fatal panic on that machine paints. The stick pull produced no
paint in sixty seconds. Therefore **no CPU panicked**, therefore none spun 500M
times on a ticket, therefore the convoy did not happen and neither did the
stranded ticket. The chain is eliminated as *this* bug; both defects it named
stay open on their own reading, because they are real and they are not this.

**What is left is a machine whose CPUs never panic, never schedule and never
answer an interrupt**, and there is exactly one state in this kernel that is all
three: halted with `IF` clear. The audit, all read from the code today:

- **`stub_halt_all` is `cli; 2: hlt; jmp 2b`** (`arch/idt/mod.rs`) — the only
  permanent interrupt-deaf halt in the tree, reached only by the `0xFD` IPI,
  which only `apic::halt_all_cpus` sends.
- **`halt_all_cpus` sends that IPI *before* it renders.** Every sibling is
  halted `IF`-clear at statement one; the paint is statement two. An initiating
  CPU that does not complete `render()` leaves seven CPUs deaf forever, nothing
  on the panel and no panic anywhere — the owner's report exactly.
- **`paint` is latched on `PAINTING`**, so a CPU that finds another mid-paint
  returns having painted nothing. `screen_claimed_by_userland` waits up to 2 s
  on that same latch.
- **Three of `halt_all_cpus`'s callers are not panics**:
  `iommu::vtd::fault::service` — *any* DMA remapping fault stops the machine —
  `scheduler::schedule_no_return`'s panicked-inside-a-pass arm, and `SYS_DEBUG`
  action 3. The first is where to look first: the T14 boots with its unit
  translating, and a device pulled mid-DMA is a way to raise a fault.
- **Checked, and not reproducible here.** `Profile::Metal` declares
  `IOMMU_DEFAULT`, so `usb_boot_stick_pulled` has been pulling the boot stick
  with the unit *translating* all along and no fault fires. That makes it a
  metal-only candidate rather than an untested one.
- **The two `IF`-clear spinlocks have no deadlock detection at all.**
  `log_ring::RingGuard::lock` and `serial::BackendGuard::lock` are `cli` plus an
  unbounded CAS spin: no bound, no contention warning, no panic. `sync::Lock`,
  which keeps interrupts *on* and is therefore survivable, has all three. **The
  machine can detect the deadlock it could live through and cannot detect either
  of the ones it cannot** — and a panic taken while holding `RING_LOCKED`
  deadlocks the panic handler's own first `log!` on that CPU, `IF` already
  clear, before it reaches `capture` or `render`.

#### Boot 1, same image, 79 seconds earlier: possibly #156 on metal

The compositor never came up. `spawned /bin/compositor pid=0`, soundd, netd,
`Boot: complete (1144ms)`, every CPU joining the scheduler — and then the boot
log, sitting there. **No probe panic fired, and by construction that means
nothing ever claimed the framebuffer**, since `CLAIMED_AT` is written only by
`screen_claimed_by_userland`.

**What the photograph cannot say**: the panel carries the boot-complete
checkpoint paint from ~1.15 s, so any userland output after that instant is
absent from it either way. It cannot separate "the compositor wedged before its
first instruction" from "the compositor exited at 1.2 s".

**What the log on the stick decides, and it is one line.** Boot 1 is
`/log/2026-08-07-114354.log`, boot 2 `/log/2026-08-07-114513.log`. Is there an
`exit: compositor` line? If yes it left, and that is an exit to explain. If the
process never appears again at all, that is **#156 on metal, in a boot rather
than after minutes of desktop use** — a process spawned and never given a first
instruction, the shape §3 has been describing from T14 logs and that nothing has
been able to stage. Far cheaper to chase than anything else in this section.

**Two readings off the same photographs that are not defects.** `i8042: armed at
1002ms, idle at 1153ms, 0 interrupts … the pin has never asserted` appears on
both boots: 150 ms after arming with nobody typing, that is the health line
doing its job, and it becomes a finding only if a *later* line in a log still
says zero. And the disk refusal works as designed — `gpt: device 1 has 4
partitions and none of them is ours`, `this disk is not ours and nothing will be
written to it`.

#### 2026-08-07, the logs: the freeze is a *boot*, and USB is not the subject

Both logs came off the stick. **The freeze reproduces at boot, with nothing
unplugged, about one boot in two.** The two boots are 79 seconds apart on one
image, and with timestamps stripped they differ only in the RTC reading, one
millisecond in two phase timings, the SMP interleaving of the `CPU N: joining
scheduler` lines, and the whole divergence: boot 2 has two `compositor: frames=`
reports and boot 1 has none.

Line-for-line identical through the framebuffer claim and past it:

```
shm: 0x4000000000 mapped WriteCombining into pid 0      1.162 s
compositor: wallpaper 1920x1080, scaling to 1920x1080
compositor: ready
spawn: /bin/filepicker pid=3 … cr3=0x1a55000            1.348 s
compositor: at most 221 windows (8 MiB each of 15402 MiB total)
```

Boot 2 continues with `frames=3 … scanout_blits=3`. **Boot 1 emits nothing
further, ever.**

**The probe's absence is what makes this the machine and not the compositor.**
`0x4000000000` is the GOP framebuffer out of `KernelArgs`, so that `shm` line is
the claim, and the claim is the only writer of `CLAIMED_AT`. Probe due at claim
+ 5 s ≈ 6.162 s; boot 2 panicked at **6.164 s**, confirming the mechanism to the
millisecond. Boot 1 shows no panic on the panel or in the log, so **no CPU
reached `probe_due()` in the idle loop after 6.16 s**. A wedged compositor with a
live kernel would have panicked. Every CPU is spinning or halted with `IF`
clear — the audit above, now with evidence.

**Two honesty constraints on reading these logs.** A log file ends at the last
successful flush and not at the moment of death — boot 2's own panic is absent
from its log for exactly that reason — so boot 1's true last event may be later
than its last line. And a *healthy* idle machine would also stop producing lines
here, since the i8042 health line only repeats once the pin has asserted. **It is
the missing probe panic that carries the conclusion, not the missing log lines.**

**Consequences for the rest of this section.** The reproduction is a boot rather
than ten minutes of desktop use, and **the unplug freeze may not be a USB defect
at all** — the same machine state, with nothing pulled out. Treat "pull the
stick" as one trigger of a general defect rather than as the subject. Everything
above about xHCI stands as correct work and none of it is the cause.

**The window is 1.35 s to 6.16 s.** Both ends are anchored on something that
survives: the last line boot 1 wrote, and the probe that was due at claim + 5 s
and did not fire. It is wider than it needs to be, and that is deliberate.

**Withdrawn, and not to be re-derived: the `compositor: frames=` cadence.** An
earlier reading of this pair narrowed the window to ~3.35 s by treating boot 2's
two-second frame-report interval as a clock boot 1 would have obeyed. That
inference is retracted. `frames=3 … scanout_wr_bytes=8370176` records *boot 2*
reaching its main loop and blitting a full screen; it says nothing about how far
boot 1 got, and nothing below may lean on it. The difference between the two
files stays an observation and stops there: boot 2 has two frame reports, boot 1
has none.

So what is in the window is stated as what the machine had just been *told to
do*, never as what it was caught doing. The scanout has been mapped
write-combining, `/bin/filepicker` was spawned one line earlier and is starting
up, and the compositor is entering a main loop whose first act is a full-screen
blit into a WC MMIO mapping. Every desktop test in the suite runs that sequence
in QEMU without freezing, so what differs is timing and the memory type, neither
of which TCG models.

#### The defect that is in that window: a TLB shootdown nobody waits for

`apic::tlb_shootdown()` writes the ICR with vector `0xFE` **and returns**. There
is no acknowledgement anywhere: `tlb::tlb_flush_entry` flushes, EOIs and
`iretq`s, and nothing counts completions. So every caller continues on the
assumption that its siblings have flushed, when a sibling may not do so for an
unbounded time — a CPU inside any `cli` region takes that IPI only when it
re-enables interrupts, and a CPU halted with `IF` clear never takes it at all.

Its eight callers are exactly the operations in the window:
`process.rs:161`, `:196` and `:588` — address-space teardown, which **frees the
physical pages** — `syscall.rs:1443` and `:1825`, and `paging.rs:256`, `:613`,
`:646`, which include the mapping whose *memory type* is being set. The log
shows all three shapes inside two seconds: `exit: netd pid=2` at 1.307 tears an
address space down, the scanout's memory type is set at 1.162, and
`spawn: /bin/filepicker` builds one at 1.348.

Two consequences, and the first is a defect on its own reading whatever the
freeze turns out to be:

- **Unmap-then-free is unsound.** A sibling holding a stale translation writes
  into a page that has already been handed to somebody else. Nothing bounds how
  long that window is.
- **A page is left mapped WC on one CPU and WB in another CPU's stale TLB.**
  That is a memory-type alias on one physical page, which Intel SDM Vol. 3A
  §11.12.4 does not permit and for which it specifies no behaviour. It is
  invisible to TCG, which models no memory types at all, and it is the one thing
  in this window that can stop a machine below the software layer — no panic, no
  schedule, no interrupt.

**Eliminated on the way**: the kernel's *two* mappings of the framebuffer are
not an alias — `gop.rs:66` and `panic_console/mod.rs:391` both map it
`CachePolicy::WriteCombining`, so the panic console and the scanout agree.

**This is a candidate and not a diagnosis.** What makes it worth ranking first
is that it is the only mechanism found so far that is present in the window, is
absent from QEMU by construction, and can produce the observed state without
reaching any software error path.

**Corroborated independently, and the other reading goes further — read it
before touching any of this.** `specs/memory-boundary-spec.md` §2.3 reached the
same conclusion from the memory-safety track on the same day, and it is the
authority for the fix: it names the same `ipi_all_excluding_self` one-ICR-write,
states that **the six existing call sites are therefore already wrong** rather
than merely incomplete — `MappedPages::release` (`process.rs:159-163`) drops the
pages after a shootdown nobody waited for — and enumerates four more sites that
free pages with no shootdown at all (`sys_munmap`, `shared_memory::{release,
destroy, unregister, cleanup_process}`, `virtio_gpu::free_framebuffer`, and
`virtio_gpu::set_resolution`). It also carries a half this entry missed
entirely: `invlpg` reads the *current* CR3's PCID (`paging.rs:196`), so
`shared_memory`'s and `virtio_gpu`'s unmap paths invalidate the wrong tag on
metal and merely the wrong CPU under QEMU.

**And it prices the fix, including a deadlock class this entry did not see.**
§3.3 is stage M3: an acknowledged shootdown with a per-CPU generation counter,
invalidation against the *target* address space's PCID, and the shootdown moved
ahead of every free. Its stated rule matters to anyone who reads the ninth-boot
experiment below as an invitation to write one: **the initiator must not wait
for acks while holding a lock a target could be spinning on with `IF` clear.**
The `IF`-clear windows are `serial.rs:98,114,163` — the serial lock under
`save_and_cli` — and IDT interrupt gates, so no `log!` may sit between issuing a
shootdown and collecting its acks. That is the same `BackendGuard` this entry's
own audit flagged as an unbounded `IF`-clear spin, arriving from the other
direction.

**M3 is memsec2's, not this task's.** The experiment below is an A/B on a
throwaway build to test a hypothesis about the freeze; it is not the fix, and it
must not be confused for the start of M3.

#### What a ninth boot should carry

Not another probe — this one is a fix-shaped A/B, and it is the cheapest
decisive experiment available. **Make the shootdown synchronous**: a per-CPU
acknowledgement counter, the sender spinning until every online sibling has
flushed, bounded, with a loud named failure when the bound is hit. Then eight
boots with it against the eight without that the owner is already collecting.
A rate that goes from about a half to zero is the answer; a rate that does not
move eliminates the strongest candidate in one round and costs one flash.

The bounded failure is half the value: a sibling that does not acknowledge
inside the bound is a CPU that is already `IF`-clear and unreachable, and saying
so *by name* turns the freeze's own precondition into a printed line — on a
machine where, as of the probe, a fatal report reaches the panel.

That change is not made here, and it is **not** M3 being started early: M3 is
`specs/memory-boundary-spec.md` §3.3 and it belongs to the memory-safety track,
which has already priced it, enumerated the sites and written the deadlock rule.
Whoever builds this A/B builds a throwaway to test a hypothesis about the
freeze, obeys §3.3's rule about `log!` between issue and ack, and lands nothing.

**Cheaper still, and it should be tried first**: the heartbeat's `mask=` field
answers a question this experiment would answer expensively. A machine dying to
a stale translation loses CPUs the way a memory corruption does — one at a time,
in whatever order they touch the bad page — while a machine dying to a global
cause loses them between two lines. One flash of an instrument that is already
built beats one flash of a fix that is not. **The first eight boots of it read
`0/8` for a hundred seconds on a healthy machine** and the entry below is why;
the field means what it says from `diag-tick` on.

#### What the owner should run — settled

**Already answered: the probe painted.** Kept as the record of what it settled.
No further reflash for that question.
`cargo run -- --console-boot --kernel-feature metal-panic-probe --build-only`
(or `--diag-boot`, or the ordinary image — the probe is orthogonal to the boot
mode). Flash it, boot to the desktop, and wait. Five seconds after a process
claims the framebuffer the kernel raises a real fatal panic from an idle CPU —
the same path, the same context and the same stack a machine-stopped panic uses.

- **The panel fills with a panic report** naming
  `metal-panic-probe` → the fatal path works on his hardware, with his
  compositor, on his panel. Every future "nothing appeared" is then evidence
  that no panic happened, and the freeze is not reaching one.
- **The panel does not change** → the T14 has never been able to report a fatal
  panic, three investigations have run blind, and that is worth more than the
  freeze because it is the prerequisite for diagnosing anything else on that
  machine.

`screen_fatal_halt_composited` gates the same feature in QEMU, so a green suite
says the software half works and the reflash asks only about the panel. That
split is deliberate: QEMU's framebuffer is host RAM, while the T14's is a
write-combining MMIO mapping the compositor is also writing and a full paint
measures ~460 ms there.

#### CLOSED — the heartbeat's first build was blind on an idle machine, and the eight boots that proved it

Eight `--console-boot --kernel-feature heartbeat --kernel-feature
metal-panic-probe` boots, logs at `2026-08-07-15{3347,3603,3819,3854,4108,4203,
4259,4532}.log`. Every one has the same shape:

```
[kernel   1.150 cpu4] heartbeat: t=1.150s passes=8/8 mask=0xff
[kernel   1.782 cpu0] heartbeat: t=1.782s passes=8/8 mask=0xff
[kernel 104.512 cpu0] !!! PANIC !!! metal-panic-probe …
[kernel 104.512 cpu2] heartbeat: t=104.512s passes=1/8 mask=0x01
[kernel 104.512 cpu5] i8042: the pin asserts — 1 interrupts, 1 bytes, 1 keys …
```

**Exactly three heartbeats per boot, in all eight**: two while userland came up,
then nothing for 14 s, 16 s, 31 s, 102 s, 115 s or 121 s depending on the boot,
then one final line in the *same millisecond* as the first keypress. The owner's
account matches — "waited a long time no panic, Fn button definitely causes
panic", "same but for caps lock" — and every trigger he found is an interrupt.

**The finding: after ~1.8 s no CPU takes a scheduler pass at all.** Every CPU's
pass found no work and no deadline, so it stopped its LAPIC timer and halted
until something external arrived. That is correct behaviour and good power
management, and it made the instrument blind at the one job it was built for:
`heartbeat::poll` ran from the idle loop, a halted CPU does not run the idle
loop, and **a healthy quiescent machine and a dead one wrote byte-identical
logs** — the precise failure the heartbeat was built to eliminate. The same
defect blinded the probe, which is why it fired 99 s late and only on a keypress
rather than at its deadline.

**Fixed by `diag-tick`** (`kernel/Cargo.toml`, `KernelHw::idle_wait`,
`apic::arm_within`): a diagnostic build caps how long a CPU may sleep at 100 ms,
taking the minimum against whatever the pass just armed so no wakeup is ever
pushed out. `heartbeat` and `metal-panic-probe` both depend on it. The line now
reads `alive=N/8` with a `gap=` field, and names each missing CPU with how long
it has been silent. Gated by `kernel_heartbeat`, whose teeth are recorded in its
own comment: with the tick removed, 10 of 11 lines dropped a CPU, six of them at
`alive=2/8`, one CPU silent for 2.811 s — and the *old* gate's assertion was a
mask that **varies**, so it was satisfied by the defect and certified it.

**What these eight boots do not establish, and must not be read as:**

- **The desktop freeze was not tested.** The owner ran `--console-boot`. Nothing
  here is evidence about #156's own reproduction.
- **The unplug case cannot be tested with this image at all.** The probe makes
  any interaction panic the machine. Boots 4 and 5 (`153854`, `154108`) are the
  two truncated at 1.8 s with no panic recorded: he pulled the stick, the panic
  fired on the pull, and the log could not be written to a removed stick. The
  machine was then in the halted pager, so *"still responding"* is
  `page_forever` polling the i8042 by design and says nothing about whether the
  unplug would have frozen a running machine.
- **A silent log still means less than death.** A set bit says that CPU took an
  interrupt, returned from `hlt` and reached a pass; the tick that produces the
  next one is re-armed by the timer stub in assembly, so the chain breaks only
  where a CPU stops taking interrupts. If every CPU's LAPIC timer stopped at
  once — a C-state that parks it, an SMI storm — the log would go quiet and read
  as death. The kernel only ever executes `hlt` and programs no C-state MSR, but
  the T14's firmware is not ours, so the honest claim from a stopped log is *the
  machine stopped taking timer interrupts*.
- **The instrument is no longer passive.** Each heartbeat carries a
  `sync_mount` of `/log`, so a `heartbeat` build touches the boot stick four
  times a second and the boot it watches is not quite the boot without it.

#### What the owner should run next — the ninth boot

```
cargo run -- --kernel-feature heartbeat --build-only
```

The **ordinary** image and not `--console-boot`: the desktop is what freezes and
the last eight boots did not run it. No `metal-panic-probe` — it makes any
keypress panic the machine, which is what stopped those boots being able to test
the unplug. Flash `target/bootable.img`, boot to the desktop, use it, and pull
the log off `/log` afterwards. **Nothing needs to be touched for the log to be
readable**, which is the whole change:

- **Heartbeats continue at ~250 ms with `alive=8/8` and `ran=` moving, to the
  end of the file** → the machine was still scheduling *and still running
  tasks* when the power went off. Previously indistinguishable from death.
- **Heartbeats continue with `alive=8/8` and `ran=0` line after line** → the
  decisive reading. The scheduler and the interrupt layer are alive and the
  failure is above them — a lost wakeup, or userland wedged — and every
  hypothesis below the software layer is out, including the shootdown. This is
  the case the `tone` boot below makes live, and it is the whole reason the next
  flash is worth making.
- **Heartbeats stop dead** → the machine stopped, at the timestamp of the last
  line ± 250 ms. That is the freeze, with a time on it for the first time.
- **`alive=` falls one CPU at a time over several lines**, each named by a
  `heartbeat: cpuN last reached one …s ago` → a local cause spreading, which is
  what a stale translation looks like: CPUs die in the order they touch the bad
  page. This is the reading §3.3's shootdown hypothesis is waiting on.
- **`alive=` goes from `8/8` to nothing between two lines** → a global cause,
  below the software layer, and the shootdown hypothesis loses its best
  evidence.
- **A `gap=` far larger than 0.250s on a line that is otherwise healthy** → the
  machine went quiet and came back rather than dying. On the eight boots above
  this was the whole file; it should now never happen.

#### The `tone` boot — 86.9 s healthy, and what the heartbeat would and would not have caught

`2026-08-07-174543.log`, 366 lines, ordinary desktop image with **no** heartbeat
feature. The owner typed `tone` into a terminal, deleted a character, let it sit,
and the machine froze; Ctrl+Alt+D did nothing afterwards. His words: *"it felt
like it died idling."* This is the first freeze with a long healthy run, a
precise last event, and a working control in the same capture.

**Observed.** Shell at 5.08 s, compositor reporting `frames=2` every ~2 s
throughout (a blinking cursor), PMM flat at 168/15402 MB across every 10 s
report, every allocator tag steady. Keys counted 4 @13.4 s, 5 @28.6 s, 12
@38.6 s (`last byte at 29115ms`), 13 @86.859 s. The log ends on three lines —
the i8042 counter from cpu0, then `sched: cpu=5` and `sched: cpu=6`, each
`ready=0 parked=1 current=None`. No dump lines at all, so Ctrl+Alt+D never
began.

**The control is real and it matters.** At 28.645 s a key produced the *identical
three lines*, and the machine carried on — the compositor's next window went
`frames=2 → 6` as the character echoed. So the last three lines of this log are
byte-shaped like a healthy keystroke, and nothing in them is a symptom.

**Correction to "died idling": the evidence says it did not.** The compositor's
2 s reports run unbroken to the end — three of them between the 81.229 s PMM
dump and the 86.859 s counter line, at ~1.9 s apart, exactly its healthy
cadence. A machine that died during the 57 s of idle would have stopped
producing those, and the log would show the gap. **It stopped at the 13th
keystroke**, within the flush window of 86.859 s. The owner's impression is
explained without contradicting him: he had stopped typing 57 s earlier, so from
his side the machine had been idle, and the key he pressed to check was the one
that coincided with the stop.

**Second correction, smaller:** `ready_len`/`parked_len` are `try_with_cpu`, so
`parked=1` is *this CPU's* count. cpu5 and cpu6 each had one parked task, and
cpu0 reported `parked=1` at 81.229 s too — three parked tasks, not one.

**Would the heartbeat have caught it? Total stoppage yes, a lost wakeup no —
and that is the honest answer.**

- If the machine stopped scheduling or stopped taking interrupts, the heartbeat
  ends at 86.859 ± 250 ms and the mask says whether the CPUs went together or
  one at a time. Caught, with a time on it.
- If the failure is a **lost wakeup** — timers still firing, CPUs still taking
  passes, a parked task never woken — every CPU still reaches the idle loop and
  the line reads `alive=8/8` for as long as the machine sits there dead. Not
  caught. Worse than not caught: the instrument would assert health through the
  freeze.

**But this log already argues against a lost wakeup confined to the input path.**
The compositor wakes on its own timer to blink a cursor; it does not depend on
the keyboard. A lost keyboard wake leaves it blinking and leaves its 2 s report
coming. Both stopped at the same instant, so whatever failed took the compositor
with it. That does not eliminate a *global* wake failure — every wake lost while
timers still fire would look exactly like this and would still print `alive=8/8`
— but it does eliminate the narrow reading.

**What `diag-tick` buys this investigation anyway, and it is not small.** In this
log cpu5 and cpu6 published their scheduler census **twice in 87 seconds** —
at 28.645 s and 86.859 s — because `log_health` runs from the idle loop and those
CPUs reached it only when a keystroke happened to wake them. Every other CPU's
state across 87 s of a freeze investigation is simply absent. With the tick, all
eight publish `ready`/`parked`/`current` every 10 s regardless of quiescence, so
the next capture carries a whole-machine census right up to the last flush
rather than a two-CPU sample taken at the two moments a key arrived.

**The instrument's own risk, stated because it is on the suspect path.** The tick
makes all eight CPUs run the idle loop ~10×/s where they previously halted, and
every iteration takes `drain_serial`'s `BackendGuard::lock` — `save_and_cli`
then an unbounded spin with no deadline and no panic (`serial.rs:97`), the one
lock §10 calls out for exactly that. On the T14 there is no serial device so each
hold is an empty drain, but the *acquisition rate with `IF` clear* goes from
near-zero on an idle machine to ~80/s machine-wide. If the next boot behaves
differently from these, that is the first thing to suspect, and it is why the
build carrying this must not be confused with the shipping one.

**So `ran=` was built, and the obvious design would not have served.** That
design is a per-CPU `(ready, parked, current)` census beside the `TICKED` stamp,
and it is wrong for a reason worth keeping: a woken task is dispatched within
microseconds, so `ready=` sampled four times a second reads 0 on a healthy
machine and 0 on a dead one. **The signal is a rate, so the instrument has to be
a counter.** `heartbeat::note_dispatch` counts tasks switched onto a CPU — from
`KernelHw::switch`'s `Some(_)` arm, the one place a task rather than the idle
context becomes what a CPU is running — and the line carries the machine-wide
delta since the previous one. Two signatures that used to be one:

- **the line stops** → nothing is scheduling; the machine stopped.
- **the line continues with `ran=0`** → the machine is scheduling and running
  nothing. A lost wakeup, or a userland that has stopped asking.

`ran=0` is not self-interpreting and the module doc says so: a machine with
genuinely nothing to do also runs nothing. It is diagnostic on the T14 because
that desktop always has something — the compositor wakes about twice a second to
blink a cursor and every one of those is a dispatch — so a *run* of `ran=0`
there is a machine that has stopped doing what it was doing. Cross-check against
the i8042 counter line, which says whether input was arriving meanwhile.

#### The third freeze — the first audio period, and why one signature was not enough

`hda-metal/2026-08-07-183104.log`, 236 lines. **An older image**: flashed after
H2/H4 landed and *before* M3's shootdown, so the defect it shows may already be
closed, and it is evidence about the shape rather than about the current tree.
The HDA driver bound on the T14 — ALC257 found, both codecs walked, speaker pin
selected, path configured — then `spawn: /bin/tone pid=6` at 3.799 s, `soundd:
opening stream: 44100Hz 2ch`, `client 0 connected`, `soundd: resumed`, `tone:
440Hz for 2s`, and nothing ever again. That banner prints *before* the first
audio callback, so the machine stopped as the HDA DMA stream started.

That makes three metal freezes with three triggers: a process reaching its first
instruction (~1.36 s), a keypress after 57 s of idle (86.9 s), and a stream's
first DMA (~3.8 s). The common factor is **something being scheduled or woken**,
which is #156's own title almost verbatim. Against the instrument as it first
stood all three would have read `heartbeats stopped at T` — a time and never a
class. With `ran=` they read as a time *and* one of two classes, which is what
makes a fourth flash worth more than the third was.

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
  Same bound.

**X2a took the two that ran inside a scheduler pass out of that list.** A
teardown's Disable Slot and an endpoint recovery's three-in-a-row (Reset or
Stop Endpoint, Set TR Dequeue, CLEAR_FEATURE(HALT)) are submit-and-return now,
so the six seconds above are reachable only from the boot path and from
`storage_read`/`storage_write` — the first has no scheduler to give a pass back
to, and the second is the case named below that this conversion does not fix.
`device::configure` is the one blocking caller `poll_if_pending` still reaches
and it is X2b's.

So a worst case is a CPU that does not reschedule for **six seconds**, and an
ordinary hot-plug enumeration on the T14 is ~14 ms of it (the entry below).
Nothing in the suite can measure the bad case: QEMU answers every one of these
in microseconds, which is why a driver built entirely out of them passed
everything here for a season.

**The conversion is the same idiom `PortWork` already uses** — the debounce and
the port reset were spins until #94 and are now states the poll returns to — so
the shape is known and the work is mechanical rather than novel. What makes it
big is its extent: `configure` is a straight line of control transfers, and it
has to become a state machine that gives the pass back between steps.
`restart_endpoint`'s half of that is done: the route is
`toyos_xhci::recovery`'s, driven twice — a blocking loop for a disk's bulk pair,
which runs on the thread that faulted, and a stepped one for HID. **The sequence
is shared and only the drive loop is not**, which is the shape `configure`
should take too.

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
pointer stream were reported as suspects. `i8042_mouse` now runs three thousand bytes of
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
`device::try_claim(DeviceType::Framebuffer)` sets it as the
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

### OPEN — the T14's firmware hands over an *uninitialised* 8042 about one boot in seven, and the fallback has nothing to stand on there

Found in `specs/metal-logs/2026-08-07-freeze/`, seven consecutive boots of one
image. Six read `cfg=0x77->0x64`. `222741` reads `cfg=0x30->0x60`, and the
driver disabled the keyboard by name:

```
i8042: kbd DISABLED - the set query answered 0xee and firmware's cfg 0x30 has
       translate off, so nothing says what the wire carries
```

**Subtract our own two commands and the number says more than the message
does.** `before` is read *after* `CMD_DISABLE_PORT1` and `CMD_DISABLE_AUX`, so
bits 4 and 5 of it are always ours. Firmware's byte was therefore `0x47` on the
six — kbd IRQ, aux IRQ, system flag, translate — and **`0x00` on the seventh**.
Bit 2 is the system flag POST sets to say it has initialised the controller. So
that boot was handed an 8042 in its power-on default: firmware had not touched
it at all.

The refusal is correct as written and this is not a request to weaken it. What
the entry records is that **the evidence the fallback rests on is not always
there**, on this machine, at a rate of roughly one boot in seven — and the cost
when it is absent is the whole of the machine's integrated input, because the
refusal returns before the aux port too. The owner sees a desktop with a dead
keyboard and a dead TrackPoint, and the one line saying why is in a log he can
only read after rebooting. In the 2026-08-07 set that boot is one of the five
"freezes", and it is not one.

Two smaller things fall out and neither is fixed:

- **The message calls `0x30` "firmware's own cfg" and two of its bits are
  ours.** The inference is untouched — bit 6 is firmware's — but the label
  overclaims, and the value a reader is asked to reason about is not the value
  the sentence names.
- **What a correct answer would even be is a policy question, not a bug.** The
  controller is put into translating mode by `wanted` regardless of what
  firmware left, so the open question is only what the *device* emits, and on
  this EC neither `0xF0 0x00` nor `0xF2` may be asked (see the entry below).
  Guessing is the outcome the read-back exists to prevent; refusing costs the
  keyboard. It is the owner's call which way this machine should fail.

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

### Three of the six USB/ESP gate holes the teeth audit demonstrated are still open

`specs/type-safety-audit/usb-gate-teeth.md` is the record — it names each hole,
gives the mutation that proved it, and its Part 3 is a ranked work list. It was
written against `8d7044c`, which `git rev-list --count 8d7044c..HEAD` puts 738
commits behind as of this entry, and three of its six have closed since. Filed
here so a reader does not re-investigate all six, and so the open ones are
visible from this file:

**Closed.**

- The ESP fsck gate's blindness to any value in the `..` entries of `/EFI` and
  `/toyos` (audit `:30-35`, `:230-274`) — closed by rewrite. `tests/common/volumes.rs:36`
  now judges with `toyos-fat32-check`, which has `Complaint::DotCluster` /
  `DotDotCluster` / `DotInRoot` and derives from neither our writer nor our
  reader (`volumes.rs:35`), and the gate is **silence rather than sameness**: a non-empty complaint
  list is refused before the guest runs (`volumes.rs:275-281`) and after
  (`:342-348`). That is the audit's own ranked item 5.
- `tests/common/usb.rs`'s needle that could never fire (audit `:43-45`) — now
  `" designated, blocks="` at `usb.rs:292-297`, with a comment naming the old
  defect.
- `healthy=true` as an asserted constant (audit `:46`, `:119-128`) — now
  `xhci::storage_online(self.index) == Some(true)` (`usb_storage.rs:73-75`) down
  to `MscDevice::online()` (`xhci/wait/msc.rs:102-104`).

**Open, unchanged.**

- **`usb_storage_gate`'s read half is certified by one in-guest comparator that
  nothing certifies.** `first_bad` (`kernel/src/usb_gate.rs:59-65`) is still the
  only comparator and `:118-131` the only verdict on the host's blocks; nothing
  prints a digest the harness could recompute. Audit ranked item 1, not built.
- **`xhci_no_interrupt`'s "nothing claimed a device" tooth passes on any absent
  line.** `tests/toyos.rs:6851-6857` is still `parse_xhci_binds(boot.text())`
  followed by `if !binds.is_empty()` — a negative over a parser, which a renamed
  log line satisfies vacuously.
- **The stamp guard has no test.** `usb_gate.rs:100-104` refuses a disk whose
  stamped block count disagrees with the device's, and `grep -rn "is stamped for" tests/`
  returns nothing: no profile stages a mis-stamped image.

Of the audit's eleven ranked items, 3, 5 and 10 are built — item 3 is
`Profile::UsbDiskCrowd` (`tests/common/qemu.rs:307`, shape at `:936`), which also
closed the harness gap that a `Shape` carried one disk triple (`usb_disks:
&'static [UsbDisk]` at `:641`). Items 1, 2, 4, 6, 7, 8, 9 and 11 are not; a
`usb-short-read` kernel feature now exists (`usb_gate.rs:147-155`,
`xhci/mod.rs:1818-1824`) which reaches a short read but not a failed device.
Nothing in this list is duplicated by *USB mass storage: what is not implemented*
below — that entry records driver gaps, these are test gaps.

### OPEN — this suite still formats and populates through two macOS binaries

**The judge is ours as of 2026-08-08.** `fsck_msdos` is gone from all three
places it was used — `src/image.rs`, `tests/common/volumes.rs` and
`toyos-fat32/tests/common/mod.rs` — replaced by `toyos-fat32-check/`, written
from fatgen103 and derived from neither our writer nor our reader. The owner's
rule that made that mandatory: "no dependencies on binaries that dont come with
rust or qemu". The two entries below — the FAT mirror and duplicate 8.3 names,
both of which `fsck_msdos` silently accepted — are among the twelve corruptions
the new checker catches and it did not.

**What is left is `newfs_msdos` and `hdiutil`**, and they are the harder half
because they are not a judge. `Image::formatted` shells out to
`/sbin/newfs_msdos -F 32` through a `hdiutil` device node, and the volumes are
populated through a real macOS `msdosfs` mount. That is the *point* of this
suite — our reader against bytes we did not write, and our writer's output read
back by a driver that is not ours — so replacing them is not "write a
formatter", it is deciding what independent implementation takes their place.
Consequence today: `cargo test` in this crate runs on the owner's laptop and
nowhere else, `host-tests.yml` is on `macos-latest` for this reason alone, and a
Linux contributor cannot run the FAT32 suite at all.

Scope, if someone takes it. `fatfs` is an ordinary crates.io crate already in
this tree's dependency graph and is a genuinely independent FAT32
implementation: it can format a volume and write files into one, which covers
both roles. What it does *not* give is what `msdosfs` gives — a second reader
written by people with no sight of our code — so a suite built on `fatfs` alone
tests our writer against one other implementation rather than against the
platform. Whether that is enough is the decision, and it is the owner's. Note
that `src/image.rs` already uses `fatfs` for exactly one thing (formatting the
empty volume), so the precedent exists and its limits are recorded there.

Nothing in the *guest* suite is affected: `tests/common/volumes.rs` needs no
formatter, only the judge.

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

**Both gaps this entry left open are closed** — `no_channel` is gone,
`SyscallError::Io` exists, and `fd::try_write`'s first-page failure says it.
The entry below has the shape and the gates.

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

### CLOSED — `vfs::FileSystem` folded a refused device into "no such file"

Task #133. Every method of the trait now answers `Result<_, SyscallError>`, and
`SyscallError::Io = 9` is the word for a device that did not do it — the ninth
variant `block.rs` had been waiting for, landed on its own commit ahead of the
rest because a change to `toyos-abi/src` diverges the sysroot witness and every
other worktree is refused until it is on main.

What the `Option`/`bool`/`u64` returns actually cost, all of it reachable from
an ordinary boot with no crafted input:

- **`fd::open` created a file over one that exists.** `CREATE` acted on the
  `None` arm of `vfs.open_file`, which was both "no such name" and "the volume
  would not say" — so one refused transfer got an empty file created over a
  real one, and the next write and flush made that permanent. It acts on
  `Err(NotFound)` and nothing else now.
- **`file_cache::read_page` returned `()`**, so a page the device would not
  give back reached a process as 4 KiB of zeros under a success. That is the
  one answer nothing above it can tell from a file that really is zeros there.
- **`FatFs::list` answered `NotFound`** for a volume that would not enumerate,
  which reads to a caller as "there is no such directory".
- `spawn` and `dlopen` reported every failure as `NotFound`, and the shared
  library fallback to `/lib/<name>` retried a *device* failure at a second
  path, turning one refusal into two log lines and the wrong verdict.

Two dead trait methods went with it rather than being converted:
`FileSystem::file_size` (no caller — `stat` is `open` + `fstat`, and `readdir`
carries sizes) and `FileSystem::delete_prefix` (four implementations, no
caller). `bcachefs_adapter::no_channel` is gone; both adapters now map their
crate's error enum **exhaustively**, which `toyos_fat32::Error`'s own doc
comment asks for by name. Corruption variants map to `Io` and not `NotFound`: a
btree node or a cluster chain that does not decode is a volume that cannot
answer, which is what a caller can do nothing different about.

Gates, both with the control watched red and green — `specs/metal-track-history.md`
has the discipline and this is what it looked like here:

- `log_backing_read_error` (`fat-backing-read-fails`) gained a *read* claim
  beside its write claim: the guest reads the page the host staged and the
  device refuses, and a success there is now the failure. Control: making
  `fd::try_read` ignore `read_page`'s error puts the file back as zeros and the
  test reds on the count in the guest's own line.
- `boot_volume_metadata_error` (`fat-boot-reads-fail`, new) is the *metadata*
  half, which the older feature cannot reach — it injects at
  `FatBacking::read_page`, the page-fault path, which touches no directory
  entry, so with it armed `open`, `read_dir` and mtime all still succeed. The
  new one is under `Fat32` itself and arms only once `mount` has returned, so
  the probe and the mount still work and the boot log's mount line is a
  load-bearing part of the verdict: an unmounted `/boot` falls through to the
  initrd and answers `NotFound` honestly, which is the string the test refuses.

**Two residuals.**

`close` cannot report `EIO`. The flush a descriptor owes is in
`fd::OpenFile::drop` now, which is the right place for it — it is the one path
a process killed by another CPU also takes — and a `Drop` has nothing to return
to the `close` syscall. So a failed flush is logged and no process is told.
Every *other* way of asking is honest: `fsync` returns the code, and a `write`
whose page the device refused returns `Io` rather than a count.

And, one layer up and outside this tree:

`std::sys::fs::toyos::stat` discards the error from `syscall::open` and returns
a hardcoded `io::ErrorKind::NotFound`, so `fs::metadata` re-creates the exact
conflation this task removed — at the userland end, where the kernel can do
nothing about it. `File::open`, `fs::read` and `fs::read_dir` all propagate
correctly; it is `stat`/`lstat` alone. The fix is three lines in the std fork
and could not be made from a worktree: `rust/` is an empty stub in every linked
worktree by design (`specs/worktrees.md` §2), so it belongs to whoever is
working in the primary checkout.

## 10. The stick's two partitions as filesystems, and the log on one of them

`/boot` and `/log` are both `kernel/src/fat32_adapter.rs` over `toyos-fat32`,
mounted from `gpt::boot_volume()` and `gpt::log_volume()`;
`kernel/src/log_file.rs` writes one file per boot to `/log`, named for the wall
clock. Gated by `esp_filesystem`, `kernel_log_file`, `log_backing_read_error`,
`boot_volume_metadata_error`, `log_partition_automount`,
`log_partition_identity` and `wall_clock_file` (`tests/common/volumes.rs`,
`tests/common/wallclock.rs`), and `toybox_cp_volume` (`tests/common/toybox.rs`).

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
