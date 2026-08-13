# The memory boundary — plan of record

> Status: **proposed**, awaiting the owner's approval. No code written.
> Covers tasks #159, #162 (a/b/c) and #158 — the address-space-lifetime and
> user-memory-access findings of the 2026-08 external analysis. Every claim
> below was re-verified against `main` at `a051a67`; where the finding turned
> out to be narrower, wider or wrong, §2 says so by name.

## 1. The principle

Every defect in this wave is one habit seen four times:

> **The kernel never reads a decision through a mapping userland can write, and
> never writes through an address userland chose without proving it first.**

Two corollaries, and every fix here instantiates one of them:

1. **A value from a page userland can write is untrusted input at every read,
   not at the first one.** It is copied once into kernel memory and validated
   there. A borrowed `&T` over such a page is a claim the compiler will enforce
   and the hardware will not.
2. **A kernel structure never shares a page with a grant.** Granularity is the
   grant's: a 2 MiB grant containing one kernel word grants the word.

The same habit is confirmed live outside this wave — virtio-net's descriptor
rings are a 2 MiB grant to the NIC daemon that the kernel then reads indices
back out of, and io_uring's ring is the same shape (`io_uring.rs:248` stores
`shm_phys` for a region the owning process holds a releasable token to). Those
are other agents' subsystems and out of scope here. Naming the rule is the part
that generalises; §7 records the two adjacent instances for `specs/issues/`
so nobody has to rediscover them.

## 2. What was verified, and where the findings were wrong

### 2.1 #159 — the finding is right and materially understates the defect

Confirmed as stated:

- `process.rs:36` — `vma_map` passes `writable = true` unconditionally, and
  every `dlopen` mapping goes through it (`syscall.rs:1741,1753`,
  `loader.rs:750,760`). `LibMemory::Shared`'s intent that the cached image is
  shared read-only (`elf.rs:47-60`) is not implemented.
- No NX bit anywhere in `mm/paging.rs`. `EFER` is written once, at
  `syscall.rs:75-76`, setting bit 0 (SCE); bit 11 (NXE) is never set.
- `mmap`'s `prot` genuinely is enforced (`syscall.rs:1311-1412`) — a different
  path, so both the project's claim and the finding are true.

**What the finding misses, and it is the larger half.** The *executable's own*
text is writable in every process, and not through the loader path at all.
`handle_page_fault` computes one `writable` for the whole 2 MiB page as the OR
across every region overlapping it:

```
kernel/src/process.rs:1327:   if region.writable { writable = true; }
kernel/src/process.rs:1424:   addr_space.lock().remap(UserAddr::new(region_start), page_alloc.phys(), writable);
```

`insert_elf_regions` does record `writable: seg.writable` per segment
(`loader.rs:418,431`), so the phdr information is already present and already
correct. It is discarded at the mapping step because the mapping unit is 2 MiB
and the segments are not.

Measured, on the 15 userland binaries this tree builds
(`readelf -lW` over `userland/target/x86_64-unknown-toyos/debug/`):

| | |
|---|---|
| binaries whose whole image fits in one 2 MiB page | 11 of 15 |
| binaries with **0 %** of their text write-protected at 2 MiB granularity | 11 of 15 |
| 2 MiB pages across the boot set | 21 |
| …of those, pages holding both writable and non-writable segments | 15 |
| total text | 30.64 MiB |
| text a 2 MiB-granular W^X could protect | 12.00 MiB (39.2 %) |

Per binary, `text_RO_at_2M`: `shell` 0.0 %, `terminal` 0.0 %, `soundd` 0.0 %,
`netd` 0.0 %, `toybox` 0.0 %, `snake` 0.0 %, `editor` 0.0 %, `paint` 0.0 %,
`filepicker` 0.0 %, `proctest` 0.0 %, `input-test` 0.0 %; `compositor` 60.9 %,
`files` 61.2 %, `sshd` 81.1 %, `doom` 92.3 %.

`toyos-ld` emits exactly three `PT_LOAD`s — RX, RW, R — each `p_align = 0x1000`
(`emit_elf.rs:1033,1043,1100`; `PAGE_SIZE = 0x1000` at `lib.rs:24`). RW and R
are contiguous and small, so every binary has exactly **one** mixed 2 MiB window
and zero-to-three pure ones. That is why the small binaries get nothing.

**Consequence for the plan: per-segment protection alone does not fix #159.**
The granularity decision is the fix, and §3.2 prices it.

Two further facts that bear on the shape:

- **The kernel executes out of the direct map.** `bootloader/main.rs:532`:
  `entry_virt = PHYS_OFFSET + kernel_phys + entry_offset`, and the direct map
  is one blanket `PAGE_PRESENT | PAGE_WRITE` over all physical memory
  (`paging.rs:818`). So the direct map is simultaneously the kernel's text
  mapping, its data mapping, and a supervisor alias of every user page.
  Kernel-side NX and kernel-side W^X are a *separate and larger* change and
  must not ride this one; §3.2 says what is in and what is out.
- **SMEP was off in every test this harness ran, and is on since `5d53aa0`
  (2026-08-06).** When this was written the harness passed
  `-cpu qemu64,+rdrand,+smap,+fsgsbase,+x2apic`, and
  `query-cpu-model-expansion` on that exact model string answered
  `smep: false`, `smap: true`, `nx: true` — so the mitigation that stops ring 0
  executing a user page existed on the metal (`smep=on`,
  `specs/reference/metal-hardware-inventory.md:543`) and in no guest test. The flip landed
  after §8's discovery run: both arms now carry `+smep`
  (`tests/common/qemu.rs:2199`). What the harness still does *not* do is assert
  it — no test reads `smep=on` out of a boot log, so deleting the argument reds
  nothing (`specs/issues/build/`). `cargo run`'s own arguments (`src/qemu.rs:88,90`)
  were never given it.

### 2.2 #162(a) — PCID: real on the metal, **unreachable in the harness**

Confirmed as stated: `alloc_pcid` (`paging.rs:247-260`) hands out 1..=4095
monotonically and on wrap flushes and restarts at 1 while live address spaces
still carry those tags; `Cr3::activate` sets `CR3_NOFLUSH` (`paging.rs:226`);
nothing revokes a tag. A second defect the finding does not name: the wrap
returns 1 and sets `next = 2`, so the *first* recycled tag is handed out while
whatever address space holds tag 1 is still live and still running.

**But**: measured on the harness's own CPU model, `qemu64` reports
`pcid: false` and `invpcid: false`. `cpu::enable_pcid` requires both
(`arch/cpu.rs:264-309`), so `PCID_ACTIVE` is false in every guest test. Every
context switch is a plain CR3 write — a full TLB flush — and `invlpg`
(`paging.rs:194-201`) takes the plain-`invlpg` branch. In QEMU there is
therefore no PCID bug *and no PCID benefit*.

Three consequences:

1. "MEASURE the cost of disabling PCID first" **cannot be measured in the
   harness**: disabling it there changes nothing. The measurement needs either
   the T14 or a guest with PCID forced on.
2. Any test written for a PCID fix is vacuous on today's machine shapes.
3. Measured: `-cpu qemu64,+pcid,+invpcid` is accepted under TCG and reports
   both true. A PCID machine shape is therefore buildable — a shape dimension
   in exactly the sense `specs/device-test-strategy.md` means.

### 2.3 #162(b) — shootdown: worse than "some sites are missing"

Confirmed, and one step further than the finding states. `tlb_shootdown` is a
single ICR write with **no acknowledgement**:

```
kernel/src/arch/apic.rs:70-80
fn ipi_all_excluding_self(vector: u8) { cpu::wrmsr(X2APIC_ICR, 0x000C_0000 | vector as u64); }
pub fn tlb_shootdown() { if X2APIC_ENABLED.load(Ordering::Relaxed) { ipi_all_excluding_self(0xFE); } }
```

So the **six existing call sites are already wrong**, not merely the missing
ones: each returns before any sibling has flushed, and `MappedPages::release`
(`process.rs:159-163`) then drops the pages. The fix is therefore "make the
primitive synchronous, then audit the sites", not "add calls".

The sites that do not shoot down at all, each returning pages to the PMM:

| site | what is freed |
|---|---|
| `sys_munmap` (`syscall.rs:1415-1439`) | `MmapRegion::_pages` on drop |
| `shared_memory::release` (`shared_memory.rs`) | region `PhysPage`s when unreachable |
| `shared_memory::destroy` / `unregister` / `cleanup_process` | same |
| `virtio_gpu::free_framebuffer` (`virtio_gpu.rs:482-487`) | both framebuffer page runs |

`virtio_gpu::set_resolution` (`virtio_gpu.rs:529-557`) is the framebuffer
revocation the review named: it unregisters the old pair — which unmaps it from
the compositor via `SharedRegion::unmap_all` — and drops the pages, with no
shootdown. Confirmed independently here; it is the same gap and it is inside
this wave's audit, not a separate task.

And the wrong-PCID half, exactly as stated: `invlpg` reads the *current* CR3's
PCID (`paging.rs:196`). `shared_memory`'s unmap paths and `virtio_gpu`'s run in
one process's address space while invalidating another's, so on the metal the
invalidation names the wrong tag. In QEMU (no PCID) it degrades to the right
tag on the wrong CPU — still wrong, differently.

**The design risk the finding does not mention.** A synchronous acked shootdown
introduces a deadlock class. `Lock::lock` disables *preemption*, not interrupts
(`sync.rs:27` → `preempt::disable`), so an IPI is normally delivered to a CPU
spinning on a lock — good, and that is what makes the ack tractable. But
`serial.rs:98,114,163` takes its lock under `save_and_cli()`, and IDT interrupt
gates run with IF=0. An initiator that waits for acks while a target spins with
IF=0 on a lock the initiator holds does not finish. §3.3 states the rule and
§6 carries it as the wave's top risk.

### 2.4 #162(c) — user_ptr: two claims narrower than stated, one instance wider

**Narrower.** `user_slice`, `user_slice_mut` and `user_slice_of` *do* validate
the whole span: each walks every 2 MiB boundary in the range and requires the
translation to stay contiguous (`user_ptr.rs:112-131, 146-163, 222-240`), and
`check_user_range` (`user_ptr.rs:54-59`) does bound the range to the user half.
Their defect is aliasing and TOCTOU, not span. It is `user_ref` and `user_mut`
that translate only the first address (`user_ptr.rs:192, 205`).

**Wider, and quantified.** Five of the seven `UserSafe` types can straddle a
2 MiB boundary — size exceeds alignment:

| type | size | align | can straddle |
|---|---|---|---|
| `u32` | 4 | 4 | no |
| `u64` | 8 | 8 | no |
| `[u32; 2]` | 8 | 4 | **yes** |
| `fd::Stat` | 24 | 8 | **yes** |
| `SpawnArgs` | 48 | 8 | **yes** |
| `RawKeyEvent` | 2 | 1 | **yes** |
| `MouseEvent` | 6 | 2 | **yes** |

And the harm is not "reads an unmapped page". `translate` returns the *direct
map* pointer of the first page; the kernel then touches `size_of::<T>()` bytes
from there, walking off the end of that 2 MiB **physical** page into whatever
physical page follows. The second user page need not be mapped at all. It is a
physical-adjacency out-of-bounds access, and two sites reach it from userland:

- `syscall.rs:229` — `user_mut::<fd::Stat>`, a 24-byte kernel **write**;
  16 bytes land in the next physical page when the pointer is 8 bytes below a
  2 MiB boundary. Reachable from `fstat`.
- `syscall.rs:262` — `user_ref::<SpawnArgs>`, a 48-byte **read**; the kernel
  then uses `argv_ptr`/`argv_len` taken from adjacent physical memory.

Two call sites (`syscall.rs:383,387`) use `user_ref::<u32>(…).is_none()` purely
as an address-validity probe and discard the reference — a signature used for
its side effect, which the doctrine's "code must not lie about itself" rule
already covers. `user_slice_of_mut` has zero callers and an
`#[allow(dead_code)]` (`user_ptr.rs:246`); it deletes.

Call sites to convert, counted: `user_str` 16, `user_slice_mut` 12,
`user_slice` 5, `user_ref` 3, `user_mut` 3, `user_slice_of` 1 — **40 total**.

### 2.5 #158 — confirmed, and it is one of *three* instances

Confirmed exactly as stated. `sys_dlopen` (`syscall.rs:1820-1834`) writes two
`u64` through a raw `translate(UserAddr::new(init_out))` with no range,
alignment or span check, and `AddressSpace::translate` (`paging.rs:453-465`)
applies no user/kernel bound while `new_user` shallow-copies the kernel PML4
half (`paging.rs:292-296`). A kernel address therefore translates: PML4 index
256+ is present, the walk finds the direct map's own 2 MiB leaf, and the caller
gets a writable kernel pointer. That is an arbitrary 16-byte kernel write.

**The findings did not name the other two, and one of them is a larger
primitive than #158's write.** `futex_wait` and `futex_wake`
(`process.rs:1202, 1217`) call `translate` on the raw syscall address with no
`check_user_range` and no alignment check, and the scheduler then dereferences
the result:

```
kernel/src/scheduler.rs:326:   if unsafe { *phys_addr.as_ptr::<u32>() } != expected {
```

**Correction — the oracle is not reachable, and I reported that it was.** An
earlier revision of this section claimed a process could pass a kernel address
and read kernel memory a bit at a time. It cannot. Both syscall arms guard the
address before calling:

```
kernel/src/arch/syscall.rs:383:  if ctx.user_ref::<u32>(UserAddr::new(a1)).is_none() { return bad_addr; }
kernel/src/arch/syscall.rs:387:  if ctx.user_ref::<u32>(UserAddr::new(a1)).is_none() { return bad_addr; }
```

`user_ref` applies `check_user_range` (`user_ptr.rs:186`), which caps the range
at `0x0000_8000_0000_0000`, and requires 4-byte alignment (`user_ptr.rs:189`).
A kernel address is refused before `process::futex_wait` is ever reached, and
those two are its only callers. The claim was wrong because I read
`process.rs` and `scheduler.rs` and did not read the dispatch arm that calls
them.

**What is actually wrong here, and it is a latent defect rather than a live
one.** The bound is a property of *the call site*, in a different file from the
function it protects, and it is spelled as an expression whose value is thrown
away. `process::futex_wait` is `pub`, takes a raw `u64`, and is safe only
because two callers elsewhere remember. Two ways that bites: a third caller —
an io_uring futex op, a kernel-internal wake — opens the oracle with no local
sign anything is wrong; and the guard *reads like dead code*, so tidying it away
would open the oracle silently. It is the same "signature promising a check it
never performs" pattern §7 lists, inverted: a check performing a promise its
signature does not make.

So futex stays in M1a's scope, demoted from *fix an exploitable primitive* to
*move a bound to where it cannot be forgotten* — which is what closing it at
`AddressSpace::translate` does anyway. That primitive has exactly eight callers
(`user_ptr.rs:65,71`, `io_uring.rs:248`, `process.rs:1202,1217,1320,1504`,
`syscall.rs:1827`), and one bound at the bottom retires the question for all of
them.

**#158 itself is unaffected and remains live.** `SYS_DLOPEN`'s arm validates
`a1`/`a2` (the path) with `user_str` and passes `a3` straight through with no
check of any kind (`syscall.rs:358-361`), so `init_out` reaches `translate`
unguarded. It is the one genuinely unvalidated address of the three, and it is
therefore M1a's first commit rather than futex.

## 3. The four stages

Each leaves `main`'s tip green and is landed separately, one pull request
each. Ordering differs from the task order; §3.5 argues why.

### 3.1 Stage M1 — the validated user-memory boundary (#162c + #158)

*The largest live attack surface, and the only stage with no TLB or scheduler
exposure. It lands first.*

**M1a is built.** Three commits: dlopen's `init_out` and the copy accessors,
the bound in `translate` with futex's signature, and the typed call sites with
`user_ref`/`user_mut`/`user_slice_of_mut` deleted.

**M1b is split in two, and the seam is `fd::try_read`/`fd::try_write`.**

- **M1b-1, built**: the `UserBytes` type, the 16 `user_str` sites, spawn's fd
  map and its env blob. Eighteen of the thirty-three.
- **M1b-2, built**: the fifteen remaining sites, and with them `user_slice` and
  `user_slice_mut`. **No accessor in the kernel returns a reference to user
  memory any more** — that is the stage's deliverable, and it is a property of
  the type surface rather than of fifteen call sites having moved.

**The leaf is where M1b-2's decision was, and the plan's instruction held.** Do
*not* bounce through a kernel buffer at the syscall boundary: a bounded chunk
loop breaks the atomicity a pipe write has today, and an unbounded copy puts a
userland-chosen length on the allocator, which is what `MAX_HEAP_ALLOC` exists
to refuse. The window is threaded down instead, so
`buf[a..b].copy_from_slice(&page[..])` became `user.write_at(a, &page[..])`:
one copy, the same one, with the reference on the kernel side where it is true.

**What M1b-2 built, where it differs from the sketch above.**

- **Two types, and the direction is the reference's mutability.** `UserBytes`
  reads and `UserBytesMut` writes, mirroring `&[u8]`/`&mut [u8]`, because a
  single type would let `fd::try_write`'s leaves store into a buffer the caller
  passed to be *read* — a distinction the tree had for free and would have paid
  to lose. `UserBytesMut` is write-only, which is one property stronger than
  `&mut [u8]` was: the kernel cannot be made to act on a value another thread
  substituted into a buffer it had already filled. `sub(off, len)` is what
  `buf[a..b]` was, and it exists on each type separately so a read-only window
  cannot produce a writable one.
- **`ByteSource` is the page cache's one concession.** `file_cache::write_page`
  is reached by a syscall carrying a `UserBytes` and by `log_file`, whose bytes
  are the kernel's own; a slice cannot express the first and a window cannot
  express the second, so the signature names the capability instead of a
  representation.
- **`Ring::read`/`write` describe the destination rather than take it.** They
  hand each contiguous run to a closure with its offset — one run, or two at the
  ring's wrap. The alternative was a staging buffer in `pipe::try_read`, which
  doubles the copy on the IPC path for no property, and the cost of avoiding it
  is that this stage touches `toyos-abi/src` and therefore claims the shared
  sysroot (`specs/worktrees.md` §3.1). That is the whole reason M1b-2 is two
  commits: everything else lands with no claim at all.
- **Four typed sites became `copy_out`, and each deleted an `unsafe` cast.**
  `SchedInfo`, `ProcessStats`, `FramebufferInfo` and `sys_query_modules`'
  `ModuleInfo` were reached by casting a validated byte slice to the struct —
  which checked the span but never the *alignment* the cast needs, so an
  unaligned pointer produced an unaligned `&mut T` and wrote through it. What
  the conversion adds is the whole-object check every `UserSafe` type already
  has; what it removes is the cast. `ProcessStats` is 128 bytes, so the
  accessors' "at most 48 bytes" claim moved to 128.
- **`sys_process_stats` copies before it removes.** The snapshot may be read
  exactly once, so a `copy_out` the kernel refused after taking it would leave
  nobody able to ask again — CLAUDE.md's rule that a failed operation must not
  have kept what it took. This is the only ordering M1b-2 changed and it has its
  own gate.
- **The serial console's CSI filter is a state machine**, because it was an
  index walk with a one-byte lookahead and the user path cannot look ahead.
  `write_bytes` hands it a slice and `write_user` hands it 256 bytes at a time
  out of the window, with the filter's state carried across the chunks — so a
  sequence straddling a chunk comes out as one that does not.

  **The byte-at-a-time version was written first and measured 3× slower to
  boot.** It has no chunk boundary at all, which is a property worth something,
  and `xhci_slow_connect` refused it: with the loop inside `feed` the guest
  reaches `xHCI: controller started` at 0.105 s (three runs: 0.104, 0.105,
  0.105), and with one call per logged byte at 0.32 s (six runs, 0.317–0.338),
  past the 0.3 s that test's actuator holds the ports empty for. The pre-xHCI
  log is 53 lines and 4,711 bytes of *surviving* output, so the cost is not one
  call per visible byte — a lossy ring drops most of what a boot logs, and what
  the number really says is that this path is a much larger share of early boot
  than its 4 KB of output suggests. Recorded because it makes `xhci_slow_connect`
  a boot-cost gate as much as a USB one, and the next agent to touch `log!`
  should expect it to answer.

**Delta from the plan: `read_at`/`write_at` are a raw `copy_nonoverlapping`,
not per-byte volatile.** What this stage exists to remove is the *reference*.
`&[u8]`/`&mut [u8]` carry `noalias` and `dereferenceable` into LLVM, and that is
what entitles the compiler to hoist a read, fold two into one, or reorder a
check past the use it guards — which is what makes "copy it once into kernel
memory and validate the copy" unenforceable while the borrow exists. A `memcpy`
from a raw pointer carries none of that: what comes back is a snapshot as of the
moment the kernel looked, exactly as a device read is. Volatile would buy no
formal guarantee either — a data race is a data race under both — while costing
the read and write path several times its throughput, because there is no
volatile `memcpy` and a word-at-a-time loop is what it degrades to. So the type
hands out no reference, and the copy stays a copy.

**M1b-1 has no new guest gate, and that is a claim about the change rather than
an omission.** Copying a path changes nothing a process can observe: the same
bytes, the same errors, the same refusals. What it changes is what is
*representable* — `user_str` can no longer return a borrow of a page userland
can rewrite while the VFS walks it. The suite is the regression gate for the
copy being right (every path syscall runs in it many times over), and the type
is the gate for the property.

Four things a reader of the plan below needs, because the built shape differs
from it or settles something it left open:

- **The #158 write was verified, not assumed.** A transient probe in the kernel
  translated its own canary's address inside a live user address space:
  `translate(0xffff80007cf7dc90)` answered `DirectMap(0x7cf7dc90)`, a writable
  kernel pointer. The direct map is built with `map_2m`, so the walk really does
  find a present 2 MiB leaf. (§2.5's withdrawn claim is why this was checked
  rather than repeated.)
- **A typed value that would cross a 2 MiB boundary is refused, not copied
  piecewise.** The plan says "validating the whole span the way the slice
  accessors already do", and the slice accessors *permit* a crossing when the
  two physical pages happen to be contiguous. A copy could have done the same —
  translate each page, copy each piece — and that was rejected for two reasons.
  It is more code for a value of at most 48 bytes that userland can always
  move; and it makes the gate unwritable, because mmap hands out physically
  contiguous pages, so a correct piecewise copy writes exactly the bytes the
  broken code wrote and no assertion can tell them apart. Refusing is one
  comparison, and it is observable.
- **`SYS_DEBUG` 10 and 11 are the actuator the dlopen gate needs**, and they are
  arms of the one test feature, so they cost no extra kernel build. A guest cannot
  read a byte of the kernel's address space, so a syscall that writes there
  leaves nothing to assert on; SYS_DEBUG 10 and 11 answer where sixteen known
  bytes are and whether they still say what the kernel put there.
- **The bound futex needed was not only the user/kernel one.** Deleting the
  dispatch arms' `user_ref::<u32>(…).is_none()` probe would have taken the
  *alignment* check and the demand paging with it, and the scheduler
  dereferences a futex word on every wake check — an unaligned one reads its
  tail out of the next physical page. `futex_word` carries both.

The arithmetic lives in `kernel/src/mm/user_span.rs`, pure and with no `crate::`
reference, compiled into `kernel-span/` for the host the way `kernel-loom`
compiles `sync.rs`. `check_user_range` was a third home for the same constant
and is gone; `in_user_half` is the one, and `sys_mmap` and the loader call it.

**Fix shape.**

- `AddressSpace::translate` refuses a non-user address. It takes a `UserAddr`
  whose name already claims this; the bound moves into the primitive so the
  eight callers cannot each forget it. `check_user_range`'s constant is the
  one already in the tree — no second copy.
- `UserAddr` gains a checked constructor used at the syscall boundary, so a
  kernel address is not silently expressible as a user one. `UserAddr::new`
  stays for kernel-computed addresses and says so.
- **Typed values become copy-in / copy-out.** Every `UserSafe` type is ≤48
  bytes, so a copy is strictly cheaper than the two lock-and-translate round
  trips the borrow already pays, and it removes the aliasing claim entirely.
  `user_ref`/`user_mut` become `copy_in::<T>() -> Result<T, _>` and
  `copy_out::<T>(&T) -> Result<(), _>`, each validating the **whole span** the
  way the slice accessors already do. Six call sites.
- **Bulk buffers keep the borrow, with the unsoundness made representable.**
  A `&[u8]` over a page userland can mutate is an aliasing-model violation the
  compiler believes; the honest type is a `UserBytes` handle with volatile
  `read_at`/`write_at` and no `Deref`. This is the bulk of the 40 call sites
  (33 of them) and the part that must be paced — see the delta below.
- `futex_wait`/`futex_wake` and `sys_dlopen`'s `init_out` route through the
  validated path. `[u64; 2]` joins `UserSafe`.
- `user_slice_of_mut` deletes.

**Deltas from the fix direction given, and why.**

- *"copy-in/copy-out **or** explicit pinned/volatile access"* — the plan takes
  **both, split by size**, rather than choosing one. Copy for typed values
  because they are small and the copy deletes the aliasing question; volatile
  for bulk buffers because copying a read/write buffer of arbitrary length into
  the kernel heap would put a userland-chosen size on the allocator, which
  `MAX_HEAP_ALLOC` exists to refuse. Pinning is *not* proposed: it needs a
  refcount on the page, which is `specs/assessments/capability-handles-spec.md`'s work and
  would make this stage depend on it.
- *"span validation for the typed accessors"* — kept, and generalised: the
  span check is written once and used by both the copy and the volatile
  accessors, so no accessor can be added without one.
- **Scope pacing.** The 33 bulk-buffer sites are mechanical but touch every
  syscall handler, and `syscall.rs` is a 2,061-line file another deep-dive is
  scheduled to decompose (code-quality review §3.2). M1 converts the typed
  accessors, the three unvalidated `translate` callers and the primitive's
  bound — the security-bearing half — and introduces `UserBytes` alongside
  `user_slice`. Converting the 33 sites is M1b, landed separately, so a merge
  conflict with the syscall split costs one stage rather than the wave. This
  is sequencing, not a smaller deliverable: both halves are in this wave.

**Gates, each with its negative control.**

| gate | asserts | negative control |
|---|---|---|
| host, pure | span/alignment/user-bound arithmetic over a table of (addr, size, align) cases including every 2 MiB-boundary straddle and the `u64::MAX` wrap | delete the straddle arm → red |
| guest | `fstat` with the `Stat` pointer 8 bytes below a 2 MiB boundary returns `BadAddress` and the following physical page is unchanged | revert the span check → the canary page changes → red |
| guest | `spawn` with `SpawnArgs` straddling returns `BadAddress` | as above |
| guest | `dlopen` with `init_out` naming a kernel address returns an error and the machine survives | revert #158's routing → the guest writes kernel memory; assert on a canary the kernel checks |
| guest | `futex_wait` on a kernel address returns an error rather than blocking-or-not | revert → the oracle answers; the test distinguishes the two outcomes |

**Built, and every control was seen red before it was seen green.**
`kernel-span`'s six host tests are the first row: each of its three arms
deleted alone reds exactly one test (straddle, bound, alignment), in 0.00 s.
`abuse_page_straddle` is rows two and three; with the straddle arm deleted it
reds twice, `fstat wrote 16 bytes past the end of the physical page it
translated` and `spawn read a straddling SpawnArgs and started Pid(3)` — and it
asserts the memory *before* the verdict, because an error return the kernel
produced after making the write is exactly what the gate is for.
`abuse_kernel_addr` is rows four and five. Its dlopen control is the transient
probe above, which changes the canary and reds the run; its futex control is
one line — `is_user_addr` returning true disables both the constructor and
`translate`'s bound — and reds on `futex_wait answered 0x0 for a kernel address
and expected=0x0`, which is the oracle answering. The dlopen half stays green
under *that* control, because `copy_out`'s whole-object check is a second bound;
it takes both removed to open it.

The canary is the point: a gate that only checks the *verdict* is vacuous here
(CLAUDE.md's rule about actuators that replace only a verdict). Each test must
observe that the out-of-bounds write did not happen, not merely that the call
returned an error.

**M1b-2's gates, and what each control was seen to say.** Its central property
is a type-surface one — no accessor returns a reference to user memory — whose
gate is the compiler and whose control does not compile, so what is gated here
is the behaviour the conversion *changed*. Two rows join `abuse_page_straddle`,
each observing the memory rather than the verdict:

| gate | asserts | negative control |
|---|---|---|
| guest | `sched_info` with its 24-byte `SchedInfo` 8 bytes below a boundary returns `BadAddress` and the next physical page is unchanged | straddle arm off for size 24 → `sched_info wrote 16 bytes past the end of the physical page it translated` |
| guest | `process_stats` refuses a straddling 128-byte `ProcessStats` **and the snapshot is still readable afterwards** | straddle arm off for size 128 → the canary changes; separately, remove-before-copy → `process_stats had already spent the snapshot on the call it refused` |

The controls were narrowed *by size* rather than deleted outright, because
`Stat` and `SchedInfo` are both 24 bytes and the pre-existing `fstat` row would
otherwise red first and stop the run — the same reason the existing rows assert
the canary before the verdict. `kernel-span`'s table grew the three new
`UserSafe` shapes (`SchedInfo` 24/8, `FramebufferInfo` 32/4, `ProcessStats`
128/8), whose sizes were measured by compiling the declarations rather than
counted by hand.

Everything else M1b-2 touched is gated by the suite as it stands, and that is a
claim about the change: `getcwd`, `readdir`, `sysinfo`, `readlink`, `random`,
`get_env`, `query_modules`, the whole read and write path and every pipe in the
system produce the same bytes and the same errors as before, so a conversion
that got one wrong reds something. `Ring`'s own host tests cover the closure
form at the wrap, which is the one place its two runs differ.

**Gate A exposure:** none. No TLB, scheduler or mapping path is touched.

### 3.2 Stage M2 — protection as a type (#159)

**Fix shape.**

- `Protection` — an enum with **no default**, the same explicitness
  `CachePolicy` has. Every mapping entry point (`map_range`, `remap`,
  `map_alloc`, `alloc_and_map`, `vma_map`) takes one instead of
  `writable: bool`, so every call site states R / RW / RX / R-X and the compiler
  refuses a site that does not. This is the ladder's *compile-checked* rung and
  is what makes `vma_map`'s unconditional `true` unrepresentable rather than
  merely fixed.
- `EFER.NXE` set on every CPU, beside `pat::init` and for the same reason: the
  bit must be on before any mapping can express NX, and an AP that missed it
  would interpret bit 63 as reserved and fault.
- **NX on user mappings only. The direct map is explicitly out of scope for
  this stage** — the kernel executes out of it (§2.1), so making it NX is a
  kernel-W^X change that needs its own linker-visible text/data split. M2
  states that boundary in the `Protection` doc comment so the next agent does
  not read the gap as an oversight.
- **Granularity: hybrid.** A 2 MiB window whose overlapping regions all share
  one protection keeps its 2 MiB PDE. A window that does not is backed by a
  4 KiB page table over the same contiguous 2 MiB physical page, one PTE per
  4 KiB with that page's own protection. The mechanism already exists —
  `guard_4k` (`paging.rs:653-697`) splits a 2 MiB leaf into 512 PTEs — so this
  is a second caller of a proven path, not a new one.
- `handle_page_fault`'s OR (`process.rs:1327`) is replaced by a **pure planning
  function**: given the overlapping regions and the window, produce either one
  uniform protection or a 512-entry protection plan. Pure, host-tested, no
  address space — doctrine §1.1.
- The `#PF PRESENT` log line (`exceptions.rs:501`) names a W^X violation as
  such. The dispatcher already treats present-page protection faults as fatal
  (`exceptions.rs:487`), so no infinite-fault path opens; only the diagnostic
  needs to keep up.

**Deltas from the fix direction given, and why.**

- *"the 2 MiB page granularity is the hard constraint… price 4 KiB, do not
  assume it"* — priced, and the measurement decides it. **2 MiB-only protects
  39.2 % of text and 0 % on 11 of 15 binaries** (§2.1). That is not a partial
  win; for two thirds of the boot set it is no win, and it would leave the
  shipped system with the same RWX text it has today while claiming W^X. The
  two alternatives:

  | option | cost | text protected |
  |---|---|---|
  | 2 MiB only | zero | 39.2 %, 0 % on 11 of 15 binaries |
  | **hybrid, 4 KiB where mixed** | one 4 KiB page table per mixed window per process — measured 15 mixed windows across the boot set, **60 KiB** total; TLB entries for those windows become 4 KiB | 100 % |
  | `toyos-ld` aligns segments to 2 MiB | three protection classes each need their own 2 MiB page: `shell` goes from 1 page to 3, **+4 MiB physical per process**, ~+40 MiB at boot against a 4 GiB guest — and it still does not cover `dlopen`'d libraries | 100 % |

  The hybrid wins on both counts and needs no linker change. Its real cost is
  TLB pressure: for the 11 small binaries the whole image is the mixed window,
  so all of their text moves to 4 KiB entries — `shell` is 1,300 KiB of text,
  about 325 pages. That fits a modern L2 STLB and does not fit an L1 iTLB.
  **This is the one number I cannot measure here**: TCG does not model a TLB,
  so only the T14 or a same-session A/B on real hardware can price it
  (CLAUDE.md's hard bar explicitly excludes TCG). M2 therefore ships the
  hybrid *and* the measurement request; if the T14 shows a cost worth caring
  about, `toyos-ld` alignment is the follow-on that shrinks the split count to
  zero for the exe while the mechanism stays for `dlopen`.
- *"per-segment protection from the ELF phdrs through the mapping path"* — the
  phdr information already reaches `Region.writable` correctly
  (`loader.rs:418,431`). The change is not plumbing it further; it is stopping
  the OR from discarding it. Saying so keeps the diff honest about where the
  bug is.
- **Added to the stated shape, deliberately not landed with M2, and landed
  separately in `5d53aa0` (2026-08-06):** turning `+smep` on in the harness. It
  is one argument (now `tests/common/qemu.rs:2199`), measured to work under TCG,
  and it closed a gap where the metal enforced something no test did — the gap
  itself was filed as **#167**. It did not ride M2, for a scheduling reason
  rather than a technical one: a global harness change that reds unrelated tests
  would block every in-flight agent's landing behind defects that are not
  theirs. The sequence was a **discovery run** on this branch, reported, and
  then a scheduled flip. §8 records what that run found.

**Gates, each with its negative control.**

| gate | asserts | negative control |
|---|---|---|
| host, pure | the window-planning function: uniform in → one PDE; mixed in → a plan whose every 4 KiB entry matches the region covering it; segments sharing a 4 KiB page → most-restrictive | flip the mixed case back to OR → red |
| guest | a process writing to its own `.text` dies, and the log names a W^X violation | revert `Protection` at the fault path → the write succeeds → red |
| guest | a process jumping into its own stack or heap dies | revert NXE → executes → red |
| guest | a `dlopen`'d library's code page is not writable, **and a second process's copy is unchanged after the first tries** | revert `vma_map`'s protection → the shared image is modified → red |
| guest | every binary in the boot set still runs (the split is correct, not merely protective) | — this is the coverage the negative controls do not give |

**Gate A exposure:** low but non-zero — the fault path changes and page tables
grow. A/B in one session against one HEAD, fast tier both sides; thorough tier
only if the fast tier moves.

### 3.3 Stage M3 — the shootdown (#162b)

**Fix shape.**

- `tlb_shootdown` becomes **acknowledged**: a generation counter per CPU, the
  initiator waits for every online CPU to publish the generation it flushed.
  The IPI handler already exists and already calls `flush_tlb_all`
  (`arch/idt/tlb.rs`); it gains the publish.
- The invalidation names the **target** address space's PCID, not the current
  CR3's. `invlpg` (`paging.rs:194`) takes the address space it is invalidating
  for; `INVPCID` type 1 (all-for-PCID) is the right primitive for a whole
  address space and type 0 for a single page.
- **The deadlock rule, stated and enforced.** The initiator must not wait for
  acks while holding a lock a target could be spinning on with IF=0. The IF=0
  windows are: `serial.rs:98,114,163` (`save_and_cli` around the serial lock)
  and IDT interrupt-gate handlers. Concretely: no `log!` between issuing a
  shootdown and collecting its acks, and the shootdown is never issued from
  inside the serial lock. The enumeration of IF=0 lock-taking sites is part of
  this stage's deliverable, in the spec, not in a comment.
- Every unmap-then-free path gains the shootdown *before* the free: `sys_munmap`,
  `shared_memory::{release, destroy, unregister, cleanup_process}`,
  `virtio_gpu::free_framebuffer`. `MappedPages::unmap_from`'s existing doc
  comment (`process.rs:148-152`) already states the obligation; the type should
  make it unrepresentable to skip, which is the `#[must_use]` it already has
  plus a shootdown token the free path consumes.

**Deltas from the fix direction given, and why.**

- *"do not synchronously invalidate… before returning pages"* — the plan starts
  one step earlier, because **the six existing shootdown calls are also wrong**
  (§2.3): the primitive never waited. Adding calls to an unacknowledged
  primitive would produce a fix that reads correct and is not. Making the
  primitive synchronous is therefore the first commit of the stage and the
  audit is the second.
- **Alternative priced and rejected for now:** deferring the *free* behind a
  quiescence epoch — the page returns to the PMM only after every CPU has been
  through a context switch — removes the ack wait and the deadlock class
  entirely. Rejected as the primary because it is a new mechanism with its own
  liveness question, and because the CPU count here is small enough that an ack
  wait is a few hundred cycles. Kept as the named fallback if gate A moves.
- **Added:** `virtio_gpu::set_resolution` is inside this stage's audit rather
  than a follow-up. It is the same gap on the same primitive and re-testing it
  after M3 is cheaper than re-opening it.

**Gates, each with its negative control.**

| gate | asserts | negative control |
|---|---|---|
| guest, SMP | a thread `munmap`s a page a sibling is spinning on; the sibling faults rather than reading through a stale entry after the page is reissued | revert the shootdown at `sys_munmap` → the sibling reads the reissued page's new contents → red |
| guest, SMP | shared-memory release: a grantee that kept a mapping loses it before the pages are reissued | revert → red |
| guest | resolution change: the compositor's old framebuffer mapping is gone before the pages are reissued | revert `free_framebuffer`'s shootdown → red |
| kernel actuator | the ack protocol itself — a feature that makes one CPU delay its ack proves the initiator actually waits | without it the initiator's wait is unobservable; this is the actuator CLAUDE.md requires where QEMU cannot stage the failure |

The SMP gates need a guest with more than one CPU and a *reliable* interleaving.
A racing test that passes by luck is worse than none, so each is written as a
rendezvous (the sibling parks on a flag the unmapper sets) rather than a sleep.

**Gate A exposure: high.** This touches every unmap and adds a wait to a path
the scheduler crosses. Fast tier A/B in one session against one HEAD; **thorough
tier at N=30 both sides** before the stage lands, per CLAUDE.md's rule for
scheduler-adjacent transitions.

#### 3.3.1 What M3 built, and where it differs from the plan above

**The protocol.** `kernel/src/shootdown.rs` holds it and names nothing through
`crate::`, so `kernel-loom` compiles it a second time against loom's atomics the
way it compiles `sync.rs`. One machine-wide generation counter and one
publication per CPU:

- `issue()` — `fetch_add(AcqRel)`. The release half publishes the page-table
  write the caller made before it; the acquire half stops the wait's first look
  at an acknowledgement being hoisted above it.
- `serve(cpu, flush)` — load the counter `Acquire`, **then** flush, **then**
  publish what was loaded `Release`. The read is before the flush and that is
  the protocol: a target that read after flushing could publish a generation
  whose page-table write its own flush had not seen.
- `served(cpu, g)` — `Acquire`, `>= g`.
- `owes(cpu)` — a `Relaxed` hint, for the poll below.

`arch/tlb.rs` is the hardware half: the local flush, the ICR write
(`apic::tlb_ipi`, now `pub(super)` so nothing outside can send an unacknowledged
one), the wait, and a deadline.

**The wrong-PCID half is fixed by flushing everything locally, not by naming the
tag.** The plan proposed `INVPCID` type 1 against the target address space. What
is built instead makes the initiator's own flush the whole TLB — correct under
every PCID configuration, the same thing the targets already do, and no payload
to pass with the IPI. A per-PCID, per-address shootdown is a real optimisation
and is not this stage; nothing has measured a need for one, and TCG cannot
(§2.2).

**The deadlock rule changed shape, and this is the substantive delta.** The plan
said: enumerate the `IF=0` windows and keep the initiator out of them. That
enumeration does not close. Every IDT gate clears `IF`, so *every lock any
handler takes* is one a target can be spinning on uninterruptibly — the
page-fault handler's address-space and process-data locks among them — and the
set would have to be re-derived whenever anybody added a lock to a handler. The
audit also found three free paths that run under a caller's lock by construction
(`teardown_bookkeeping` under the process table, `release_thread_mappings` under
process data, `free_framebuffer` inside `&mut self` on the GPU), so an
initiator-side rule meant threading the obligation through unrelated signatures.

So **the target answers instead of the initiator abstaining**: `Lock::lock`'s
spin calls `arch::tlb::poll` on every turn, which serves any outstanding
shootdown. A flush takes no lock, allocates nothing and is safe from anywhere, so
a CPU waiting for a lock with interrupts disabled acknowledges as promptly as one
that took the interrupt. That closes the class structurally, for locks nobody has
written yet as much as for today's.

Two things it does not close, both recorded rather than papered over:

- **An `IF=0` spin that is not a `Lock`.** A driver waiting on a device register
  inside a handler cannot poll. That is latency and not deadlock, because each
  carries its own deadline — but xHCI's is 2 s and it runs from `drain_irqs`
  (`specs/issues/kernel/`), so `ACK_TIMEOUT_NS` is 5 s and a CPU past it is named in a
  **panic** rather than waited for forever. This makes that existing defect cost
  something visible on the mapping path, which is an argument for closing it and
  not a reason to lower this bound.
- **Bring-up.** An AP counted by `CPU_COUNT` spins on `SMP_READY` with `IF`
  clear until the idle loop, and that spin is not a `Lock` either. So the wait is
  off until `smp::set_ready`, and each AP calls `arch::tlb::join` — one `serve` —
  on the far side of that spin. The acquire on `SMP_READY` makes every
  page-table write the BSP made visible first, so the join settles retroactively
  every shootdown issued while that AP was deaf.

**`mm::Unmapped<T>` is the type the plan asked for.** `Drop` shoots down and then
drops the value; `reclaim` shoots down and hands it back; there is no third way
to the value. So the obligation is discharged by the type rather than by
`MappedPages::unmap_from`'s doc comment, which is where it was. CLAUDE.md's
caveat about `Drop` guards passes here on its own terms: the value lives on the
stack of the CPU that did the unmap, on an ordinary path, and that CPU reaches
its own drop.

**Call sites, all fifteen.** Six were unacknowledged; five freed with no
shootdown at all; four more are consequences of the audit.

| site | was | is |
|---|---|---|
| `MappedPages::release` | unacked | `Unmapped<PageAlloc>` |
| `release_thread_mappings` | unacked | returns `Vec<Unmapped<PageAlloc>>`, dropped outside the process-data lock |
| `revoke_pipe_maps` | unacked | acked, outside the address-space lock |
| `sys_mmap` (`MAP_FIXED`) | unacked | acked |
| `sys_dlopen` (shared image) | unacked | acked |
| `alloc_pcid` (wrap) | unacked, **under `NEXT_PCID`** | acked, outside the lock |
| `sys_munmap` | **nothing** | `Unmapped<MmapRegion>`, dropped outside the fd-owner lock |
| `shared_memory::release` | **nothing** | `Unmapped`, dropped outside `REGIONS` |
| `shared_memory::destroy` | **nothing** | as above |
| `shared_memory::cleanup_process` | **nothing** | returns `Unmapped<Retired>`; `teardown_bookkeeping` hands it to both callers, who drop it outside the process table |
| `shared_memory::unregister` | **nothing** | returns `Option<Unmapped<Retired>>` |
| `virtio_gpu::free_framebuffer` | **nothing** | unregister, drop the wrappers, *then* drop `FbAlloc` — the order was inverted |
| `virtio_gpu::set_resolution` | — | its only free path is the row above |
| `paging::map_mmio` | unacked, **under the kernel address-space lock** | a free function: lock, map, drop the guard, shoot down |
| `AddressSpace::unmap_mmio` | unacked | **deleted** — it had no callers |

`map_mmio` earns its own paragraph. `map_2m` may replace the boot map's own
leaf, so a window inside a range the memory map covers *changes memory type*
there — write-combining for the framebuffer above all. That is §2.3's alias, on
the direct map, on a path every driver takes; eleven callers wrote the
lock-and-map incantation by hand and not one of them could have got the second
half right.

**Gates, and every control was watched red before it was watched green.**

| gate | asserts | negative control, and what it said |
|---|---|---|
| loom | an acknowledged flush postdates the page-table write | `serve` reads the generation *after* flushing → red, "cpu 1 acknowledged the shootdown while still holding a translation for the page the initiator is about to free" |
| loom | as above | the acknowledgement published `Relaxed` → red, same message |
| loom | as above | `served` reads `Relaxed` → red, same message |
| loom | as above | `issue` publishes `Relaxed` → red, same message |
| loom | one serve answers two concurrent shootdowns, which is what makes the vector's single pending bit sufficient | each of the four above → red on the first or the second initiator |
| loom | the models are not vacuous | a `served` that never answers → red, "no interleaving completed the wait, so the assertion never ran" |
| guest | a shootdown with the last CPU answering 20 ms late costs the kernel ≥ 10 ms | delete the wait → the number collapses to one ICR write |
| guest | `munmap` and a fixed `mmap` each cost ≥ 10 ms under the same arming | remove either site's shootdown → it returns in microseconds |
| guest | disarmed, the same `munmap` is back under 10 ms | — this control is *inside* the test, so the three rows above cannot pass on a kernel that is merely slow |

Loom needs a preemption bound of 2. Unbounded, neither model finishes — over
seven minutes with no verdict, the same wall the lock models hit — and at two
both run in about ten seconds and still catch all five controls, which is the
check that matters.

**What is deliberately not gated, and why.** The plan asked for SMP guest tests
observing the harm: a sibling reading through a stale translation into memory the
PMM had reissued. That is not constructible under TCG in this kernel, for three
independent reasons, and a test that passed for the wrong reason would be worse
than none:

1. the **correct** outcome is a fault, which kills the process doing the
   observing — the pass condition would be "the child died", indistinguishable
   from any other crash;
2. a context switch writes CR3, which flushes the whole TLB (no PCID under TCG,
   and no `PAGE_GLOBAL` anywhere in the tree), and a spinning sibling is
   preempted within milliseconds — so a stale entry evaporates on its own;
3. even the *unacknowledged* IPI this stage replaced landed within microseconds,
   so the window the defect leaves open is far below anything a guest can
   schedule into.

What is gated instead is the property that closes the window — the free happens
after the flush — measured where it is observable, which is the duration of the
syscall while a target answers late. The harm stays real on hardware for exactly
the reason it is unobservable here: a real TLB, a real memory type, and no CR3
write between the free and the sibling's next access.

**Gate A, measured in one session on 2026-08-07.** Fast tier: **7 of 7 green**
with the wait and 7 of 7 with it deleted, and *both* arms needed one confirm
re-boot on one config — so the transient gap is not the wait. The comparable
counters move less than the arms differ from each other: `audio_tone_load smp=1`
wake lateness 5507 µs with the wait against 6301 µs without.

The **thorough tier is red, and it is red on `main` too** — 7 dropout runs of 28
there against 5 of 12 on this branch and 5 of 40 with the wait deleted, all three
failing the same `0 of 120` recorded sample. That is a finding about the estate
rather than about M3, it is written up in `specs/issues/audio/` with the numbers, and
it is why this stage's thorough tier is an A/B rather than a pass/fail: a gate
that is red on `main` cannot say anything about a branch by being red on it.

**Is the memory-type alias now impossible?** On the paths the kernel controls,
yes: every mapping change that alters a memory type — `map_mmio`'s re-type of a
boot-map leaf, `shared_memory`'s write-combining framebuffer grant, and every
unmap that lets a physical page be reissued under a different policy — returns
only after every online CPU has flushed. What is left is *unlikely rather than
impossible*: a CPU inside an `IF=0` device spin that is not a `Lock` delays its
acknowledgement rather than skipping it, so the window closes late rather than
not at all; and `AddressSpace`'s own drop at process teardown still frees page
tables with no shootdown, which is sound today only because no PCID means every
context switch flushes, and which M4 must revisit when tags become owned.

### 3.4 Stage M4 — the PCID as an owned resource (#162a)

**Fix shape.**

- A free-list allocator: a tag is *taken* by `AddressSpace::new_user` and
  *returned* by `AddressSpace`'s drop. Exhaustion is a refusal (spawn fails
  with `ResourceExhausted`), not a silent recycle. A returned tag is flushed —
  `INVPCID` type 1 on every CPU, which the M3 primitive already provides — before
  it is reissued.
- `Pcid` is a move-only handle. CLAUDE.md's caveat about `Drop` guards applies
  and, unusually, *passes*: an `AddressSpace` is dropped by a process table
  `remove` (`process.rs:631,660,721` — the two `waitpid` reaps and the orphan
  sweep) on the reaping CPU, not by a killed thread's stack unwinding, and
  `retire_task` already rendezvouses with the dying thread
  before the payload is released (`scheduler.rs:340-358`). The plan states this
  explicitly rather than assuming it, because "which paths does this bind" is
  the question the principle demands.
- A **PCID machine shape** in the harness — `-cpu qemu64,+pcid,+invpcid`,
  measured to work under TCG — used by this stage's tests. Not turned on
  globally: PCID changes TLB behaviour under every test and would move gate A's
  recorded sample for reasons unrelated to any code change.

**Deltas from the fix direction given, and why.**

- *"disable PCID (MEASURE the cost first) or track live ownership"* — **the
  measurement as posed is not available.** Disabling PCID in the harness is a
  no-op (§2.2), so the only honest measurements are the T14 or a forced-PCID
  guest, and TCG's numbers would not count against CLAUDE.md's 2× bar anyway.
  Rather than block on hardware, the plan takes the third option — make the tag
  an owned resource — which is correct under either answer and removes the
  question. If the T14 later shows PCID earns nothing, deleting it is a smaller
  change *after* this stage than before it, because the ownership makes the
  deletion mechanical.
- **Sequenced last** because a PCID bug is only *observable* once M3's
  shootdown is correct: with an unacknowledged shootdown, a test that sees
  stale translations cannot say which of the two defects produced them.

**Gates, each with its negative control.**

| gate | asserts | negative control |
|---|---|---|
| host, pure | the allocator: take/return/exhaust, no tag issued twice while live, exhaustion refuses | remove the free-list check → red |
| guest, PCID shape | 4,096 sequential spawns do not put two live address spaces on one tag — asserted by the kernel, not inferred | restore the wrapping allocator → red |
| guest, PCID shape | the boot completes with PCID on (the shape works at all) | — coverage |

**Gate A exposure:** none on the default shape, since the PCID shape is separate.

### 3.5 Why this order

The task order was #159, #162, #158. The plan lands M1 (#162c + #158) first for
three reasons: it holds the most directly exploitable primitives (an arbitrary
kernel write and a kernel read oracle); it is the only stage with no gate A
exposure, so it lands while three other agents are moving; and #158 was already
stated to depend on it. M2 (#159) is the largest diff and lands next while the
TLB work is still being written. M3 before M4 because M4's tests cannot
attribute a failure until M3 is correct.

## 4. What this wave does not do

Named so the gaps are decisions rather than oversights:

- **Kernel-side W^X and an NX direct map — task #166.** The kernel executes out
  of the direct map (§2.1); separating its text needs linker-visible section
  bounds and is its own task. What M2 leaves for it, so #166 need not
  re-derive any of it:

  - **`Protection`, with no default**, already threaded through every user
    mapping entry point. #166 adds the direct map's callers, and the compiler
    names them rather than a grep: `map_2m`/`unmap_2m` (`paging.rs:709,725`)
    and `map_mmio`/`unmap_mmio` are the whole set.
  - **`EFER.NXE` already set on every CPU**, so bit 63 is expressible at all.
    Without it a mapping asking for NX faults as a reserved-bit violation, and
    that is the trap #166 would otherwise fall into first.
  - **The 2 MiB → 4 KiB split**, generalised out of `guard_4k`
    (`paging.rs:653-697`) into a reusable operation. The kernel's text will not
    be 2 MiB-aligned either, so #166 needs exactly this and it will exist.

  What #166 still has to establish, and M2 deliberately does not:

  - **The kernel's own text extent.** The direct map is built by one blanket
    loop over all physical memory (`paging.rs:816-820`), so nothing in the
    kernel currently knows where its own text ends. This needs symbols from
    `toyos-ld` and a decision about whether the bootloader or the kernel
    applies the split — the kernel is already executing out of that mapping
    when it would change it, which is the interesting part.
  - **The direct map as a supervisor alias of every user page.** SMEP does not
    cover it: SMEP stops ring 0 executing a *user* (U=1) page, and the direct
    map's alias of the same physical page is a supervisor mapping. So
    "userland writes a page and the kernel is induced to execute it" is closed
    by an NX direct map and by nothing else in the tree. That is the security
    argument for #166 and it is not visible from the finding text.
- **Refcounted pages / pinning.** `specs/assessments/capability-handles-spec.md`'s work.
  M1's volatile accessors are chosen precisely so this stage does not depend
  on it.
- **virtio-net's rings, and io_uring's.** §7 records both; neither is fixed
  here.
- **`user_slice`'s 33 bulk call sites** land as M1b, in this wave.

## 5. Cost

| stage | kernel diff, estimated | new host tests | new guest tests | gate A |
|---|---|---|---|---|
| M1a | **measured: 337+/144− across 7 kernel files** (estimated ~300) | 1 module, 6 tests | 2 programs, 5 properties | none |
| M1b-1 | measured: 226+/122− across 3 kernel files | — | — | none |
| M1b-2 | measured: 465+/228− across 10 kernel files, `toyos-abi/src/ring.rs` and one test program | 3 rows added to the span table | 2 properties added to `abuse_page_straddle` | none |
| M2 | ~500 lines, `paging.rs` + fault path + `toyos-ld` none | 1 module | 4 | fast A/B |
| M3 | ~350 lines, `apic.rs`/`paging.rs` + 6 audited sites | — | 3 + 1 actuator | fast + thorough A/B |
| M4 | ~150 lines | 1 module | 2 | none |

Every unbuilt figure in this table is an estimate and says so. The measured
numbers in this document are the ELF layout table (§2.1), the CPU feature
answers (§2.1, §2.2), the `UserSafe` sizes (§2.4), the call-site counts (§2.4),
M1a's row above and the translate probe in §3.1.

## 6. Risks

1. **The ack deadlock (M3).** Highest. Mitigated by the stated lock rule, the
   enumeration of IF=0 windows, and the epoch fallback. A `DEADLOCK at …` panic
   from `sync.rs:42` is the symptom and it is loud.
2. **TLB pressure from the 4 KiB split (M2)** is unmeasurable on this host.
   Carried as an open question for the T14 rather than guessed at.
3. **`+smep` reds unrelated tests (M2).** Measured against current main and it
   does not (§8). Treated as findings if it ever does.
4. **Merge pressure on `syscall.rs`** with the scheduled decomposition. M1b
   exists to bound it.
5. **Gate A's known intermittent red** (`audio_tone_load smp=1`,
   `specs/issues/audio/`) will appear during M2 and M3 A/B runs. Stash-and-re-run
   before attributing it, as the rule says.

## 7. For `specs/issues/isolation/`, not fixed here

- **io_uring's ring is a grant the kernel reads through.** `io_uring::create`
  stores `shm_phys` (`io_uring.rs:248`) for a `shared_memory` region whose token
  the owning process holds; the kernel reads the SQ head/tail through it on
  every `enter`. `claim_sqe` does bound the index (`available > sq_size` is
  refused, and the index is masked), so the ring is not the unbounded-index
  shape — but whether the owner can `release` the token and free the pages under
  the live instance needs checking, and was not checked here.
- **`dump_crash_diagnostics`'s `read_user`** (`process.rs:1502-1506`) translates
  without `check_user_range`. Crash-path, kernel-supplied addresses, so not
  userland-reachable — but it is the same shape and should route through M1's
  primitive when it is touched.
- **`user_ref` used as a validity probe** (`syscall.rs:383,387`), discarding the
  reference. Doctrine §1.5 / "code must not lie about itself".

## 8. The `+smep` discovery run

Run on this branch at `f6e48db` with the single argument added to
`tests/common/qemu.rs:1889`, to answer whether the flip can be scheduled
without landing defects into five in-flight agents' gates (#167).

**It reds nothing. 262 passed, 262 total, 0 failed, 0 INVL, 466.3 s.**

The wall clock is roughly four times the 109 s CLAUDE.md records, because five
other worktrees were spending the same twelve guest slots; it is contention, not
a finding.

The run is only worth anything if SMEP was actually on, so that was checked
rather than assumed — the kernel reports its own answer at boot:

```
[kernel 0.000 cpu0] percpu: BSP cpu_id=0 lapic_id=0 smep=on smap=on pcid=off
```

That line is also the direct confirmation of §2.2: `pcid=off` in the guest, from
the kernel rather than only from the QMP model query. The two independent
measurements agree.

**That first run was against a stale base, and its coverage claim did not
hold.** It ran on `a051a67`, and `desktop_window_child` was added to main four
commits later (`d49883e`) — so "reds nothing" was a statement about a tree that
did not contain the test most likely to be disturbed by a CPU-feature change.
Three other new tests were missing from it too.

**Nothing in the harness would have said so.** The run reported 262 of 262 and
exited 0; a suite reports on the tests it has, and a test absent from the tree
cannot be counted as missing. It surfaced only because a message named a test
whose string appeared nowhere in the log. The general lesson for anyone reading
a green suite as evidence: the number of tests that passed is not a coverage
claim, and a discovery run is dated by the base it ran on, not by the day it was
run.

Re-run after merging main:

**256 passed, 1 failed. The one red is `desktop_window_child`.**

That test is documented on main as `EXPECTED RED, pending #156`
(`specs/issues/kernel/`) and was verified red on main *without* `+smep`, so the flip is
not its cause. Two things were checked rather than assumed before accepting
that:

- **The signature matches.** The assertion interpolates the guest's serial
  output since the keystroke (`tests/toyos.rs:4011-4012`), and the dump is
  empty — the guest emitted nothing for 20 s. That is #156's freeze, not an
  assertion failing with output present, which would have been a different
  defect wearing the same test's name.
- **It is intermittent, not positional.** Under a one-test filter it still
  failed the wide phase and then passed the harness's own re-run-alone, and it
  failed in round 1 in the full suite and round 2 alone. The round moving is
  itself evidence of a race rather than a deterministic failure.

Conclusion: the flip is free, now measured against a tree that contains the test
that could have contradicted it. It is held as its own commit (`5d53aa0`) so it
can land whenever it is scheduled rather than riding M2 — a green discovery run
removes the risk that motivated the hold, but not the call on when a shared
resource changes under five agents.

**Landing this wave's stages while #156 is open** needs nothing special.
`desktop_window_child` is declared in
`EXPECTED_FAILURES` (`tests/toyos.rs`), so it still runs, it is named with its
task in every run's report, and it does not red the gate. Any other red belongs
to the change and is explained, never excluded — and the declaration cannot
absorb one, because an entry covers named failure messages and nothing else.
