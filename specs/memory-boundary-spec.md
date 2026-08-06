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
that generalises; §7 records the two adjacent instances for `known-issues.md`
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
- **SMEP is off in every test this harness runs.** The harness passes
  `-cpu qemu64,+rdrand,+smap,+fsgsbase,+x2apic` (`tests/common/qemu.rs:1889`).
  Measured via `query-cpu-model-expansion` on that exact model string:
  `smep: false`, `smap: true`, `nx: true`. The T14 reports `smep=on`
  (`specs/metal-hardware-inventory.md:543`). So the mitigation that stops ring 0
  executing a user page exists on the metal and in no guest test. Measured:
  `-cpu qemu64,…,+smep` is accepted and reports `smep: true` under TCG.

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

A process passing a kernel address gets a **binary oracle over all of kernel
memory**: `futex_wait` returns immediately when the word differs and blocks when
it matches. Roughly 32 syscalls per word read, no crash, no trace. It is in
scope because it is the same defect and the same fix; `AddressSpace::translate`
has exactly eight callers (`user_ptr.rs:65,71`, `io_uring.rs:248`,
`process.rs:1202,1217,1320,1504`, `syscall.rs:1827`) and closing the bound at
the primitive closes all three at once.

## 3. The four stages

Each leaves `main`'s tip green and is landed separately with
`cargo run -- --land`. Ordering differs from the task order; §3.5 argues why.

### 3.1 Stage M1 — the validated user-memory boundary (#162c + #158)

*The largest live attack surface, and the only stage with no TLB or scheduler
exposure. It lands first.*

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
  refcount on the page, which is `specs/capability-handles-spec.md`'s work and
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

The canary is the point: a gate that only checks the *verdict* is vacuous here
(CLAUDE.md's rule about actuators that replace only a verdict). Each test must
observe that the out-of-bounds write did not happen, not merely that the call
returned an error.

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
- **Added to the stated shape, and deliberately not landed with M2:** turning
  `+smep` on in the harness. It is one argument (`tests/common/qemu.rs:1889`),
  measured to work under TCG, and it closes a gap where the metal enforces
  something no test does — the gap itself is filed as **#167**. It does not
  ride M2, for a scheduling reason rather than a technical one: a global
  harness change that reds unrelated tests would block every in-flight agent's
  landing behind defects that are not theirs. The sequence is a **discovery
  run** on this branch, reported, and then a scheduled flip. §8 records what
  that run found.

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
- **Refcounted pages / pinning.** `specs/capability-handles-spec.md`'s work.
  M1's volatile accessors are chosen precisely so this stage does not depend
  on it.
- **virtio-net's rings, and io_uring's.** §7 records both; neither is fixed
  here.
- **`user_slice`'s 33 bulk call sites** land as M1b, in this wave.

## 5. Cost

| stage | kernel diff, estimated | new host tests | new guest tests | gate A |
|---|---|---|---|---|
| M1 | ~300 lines, `user_ptr.rs` + 3 call sites | 1 module | 5 | none |
| M1b | ~200 lines across `syscall.rs` | — | — | none |
| M2 | ~500 lines, `paging.rs` + fault path + `toyos-ld` none | 1 module | 4 | fast A/B |
| M3 | ~350 lines, `apic.rs`/`paging.rs` + 6 audited sites | — | 3 + 1 actuator | fast + thorough A/B |
| M4 | ~150 lines | 1 module | 2 | none |

Every figure in this table is an estimate and says so. The measured numbers in
this document are the ELF layout table (§2.1), the CPU feature answers (§2.1,
§2.2), the `UserSafe` sizes (§2.4) and the call-site counts (§2.4).

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
   known-issues §4) will appear during M2 and M3 A/B runs. Stash-and-re-run
   before attributing it, as the rule says.

## 7. For `known-issues.md` §1, not fixed here

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
(known-issues §3) and was verified red on main *without* `+smep`, so the flip is
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

**Landing this wave's stages while #156 is open** needs nothing special:
`cargo run -- --land`. `desktop_window_child` is declared in
`EXPECTED_FAILURES` (`tests/toyos.rs`), so it still runs, it is named with its
task in every run's report, and it does not red the gate. Any other red belongs
to the change and is explained, never excluded — and the declaration cannot
absorb one, because an entry covers named failure messages and nothing else.
