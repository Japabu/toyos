# Code-quality review, 2026-08 — verdicts and target states

The owner reviewed the tree and left 35 `// REVIEW:` notes (branch `jan-review`,
commits `60855a4` and `d5b492e`). Every note was walked with him in session on
2026-08-05/06 and every verdict below was agreed. **Nothing here is scheduled
work.** This records the destination — where the code should end up, decided
while explicitly refusing to plan the route — so later deep-dive agents (§3)
start from settled ground instead of relitigating it. Effort was not an
argument in any verdict.

**Status marks are as of 2026-08-08, `main` at `163ee95`**, and every number in
them was measured on that tree. A verdict with no mark is unchanged since it was
written. `jan-review` is deleted; §5 is the note ledger that replaces it, so
nothing is reachable only through a branch.

## 1. The doctrine

Seven commitments, each already proven somewhere in the tree and now adopted as
the target everywhere.

1. **Decisions are pure and host-tested; the kernel keeps only effects.** The
   full ladder: unrepresentable > compile-checked > host-tested > guest-tested,
   with a guest boot reserved for what genuinely lives at the hardware
   boundary. Host tests are preferred because they are faster — the owner's
   explicit constraint.
2. **Every format is a crate.** `toyos-gpt`, `toyos-ps2`, `toyos-fat32`,
   `toyos-keymap` exist; `elf` (parse half) has since joined (`e2c6a06`);
   `symbols` and `acpi` still owe. The ladder ends at bcachefs. Fixtures are
   real artifacts; fuzzing the trust boundary happens on the host where it is
   cheap.
3. **Every state machine has a host model.** The scheduler has one
   (`toyos-sched`); the xHCI port machine has since joined (`toyos-xhci`,
   `2e81ae8`); process lifecycle and the USB BOT transport still owe. The
   2026-08-05 T14 session's three worst bugs (retire race, SS-port wedge, spawn
   wedge) were all interleaving or state-space bugs in unmodeled machines.
4. **One contract per boundary, stated once.** PerCpu offsets flow from
   `offset_of!` into asm via `const` operands; vector numbers exist once, in
   the enum; syscall argument decoding is one visible layer; sentinels only at
   wire formats, decoded at the boundary.
5. **Code speaks; specs carry narrative; comments carry only what code cannot
   say.** Three comment kinds survive: the one-clause invariant at the edit
   site, the boundary contract, and the refusal-reason at a surprising
   decision. Module doc = contract plus one spec pointer, target ten lines.
   Narratives, measurements, history → commit messages and specs. The kernel
   never names a hardware model — the bound stays, the machine that produced
   it lives in git history.
6. **Test machinery is quarantined.** Feature-gated actuators are correct
   (`usb_gate` and `input_merge_test` are `#[cfg]`-gated at declaration and
   call — never compiled into an ordinary build; re-verified 2026-08-08 at
   `main.rs:27-30`, `:446`, `:566`), but they live interleaved with production
   sources; target is one directory (e.g. `kernel/src/gates/`) so what test
   machinery exists is auditable in one listing. An actuator must name whether
   its subject is hardware (stays) or pure logic hiding behind hardware
   (extract the logic, host-test it, shrink or delete the gate).
7. **A TODO in code is either a filed task or a fix.** Two TODOs in
   `mm/region.rs` sat invisible until this review tripped over them. Still
   exactly two, and still the only two in first-party Rust — measured
   2026-08-08 across `kernel/ userland/ toyos/ toyos-abi/ bootloader/ src/`.

Consequence worth stating: CLAUDE.md's "minimal kernel" claim, currently
false (the kernel holds two filesystems, four virtio drivers, xHCI, NVMe,
i8042, GOP, a page cache, io_uring), becomes structurally true as logic
migrates to crates and models — the kernel trends toward an effects layer.

## 2. Verdicts, by area

File:line references were as of `jan-review` (`d5b492e`, based on `d5174a5`);
where a file has moved since, the mark says where it went.

### arch/

- **`gdt.rs`** — **OPEN, and larger than the verdict recorded.** A re-export
  shim; the verdict said one caller of `KERNEL_CS`, and it is now four
  constants (`KERNEL_CS`, `STAR_SYSRET_BASE`, `USER_CS`, `USER_DS`) across five
  call sites in `arch/syscall.rs:107` and `loader/start.rs:60,61,85,86`. Delete
  still stands; the callers import from `percpu`.
- **`apic.rs` `init_timer_ap()`** — **OPEN.** Still empty at `apic.rs:269`, one
  caller at `smp.rs:269`. Correct behaviour (calibration is one global
  measurement; `arm_one_shot` programs divide/LVT per call). Delete the
  ceremony. May legitimately return for heterogeneous ARM cores; that day
  reintroduces it.
- **`arch/mod.rs` `allow(dead_code)` on `debug`** — **OPEN, count confirmed.**
  Masks exactly five uncalled investigation tools (`set_context`,
  `watch_write`, `clear`, `monitor_pte`, `check_pte_monitor` — zero callers
  each) while `read_dr6` and `context` have one each, from the #DB handler.
  Delete the five (git history is the shelf), keep the two, drop the allow.
  Folds into the crate-level `allow(dead_code)` entry, now filed as
  `specs/issues/build/` by `451edff`.
- **`mtrr.rs`** — keep as-is, no new comment. It is the read-only decoder
  behind the one log line that diagnoses a mis-typed scanout on any future
  machine; the owner's question was a knowledge gap, not a code defect.
- **`percpu.rs` offsets** — **OPEN, and the number is worse than recorded.**
  The verdict said "twenty-plus hand-written `gs:[N]` copies"; measured
  2026-08-08 there are **47, across eight files** (`preempt.rs` 13,
  `arch/percpu.rs` 9, `arch/syscall.rs` 8, `arch/idt/mod.rs` 5,
  `arch/idt/timer.rs` 5, `log.rs` 3, `arch/idt/device_irq.rs` 2,
  `arch/idt/tlb.rs` 2), naming 15 distinct offsets, against 14 `const _: () =
  assert!(offset_of!(..) == N)` that pin the struct to literals and bind none
  of the 47. Target: `offset_of!` fed into asm as `const` operands — one
  source, asserts delete. **Must precede any PerCpu field surgery.** Filed as
  `specs/issues/design-debt/`.
- **`percpu.rs` fields** — **OPEN, confirmed.** All 22 consumed except
  `lapic_id`, which still has zero readers outside the file (`percpu.rs:81`
  declared, `:270` written, never read); drop it after the offset unification.
  The syscall rip/num/rbp trio stays: three stores per entry buy every panic
  the ability to name where userland was.
- **`percpu.rs` init** — **OPEN, confirmed.** `alloc_percpu` sets 8 of 22
  fields and relies on `alloc_zeroed` for the rest; becomes one total
  `ptr::write(PerCpu { .. })`. The tid/pid `u32::MAX` sentinel stays: asm wire
  format, already `Option`-decoded at the boundary.
- **`idt/`** — **DONE, `9bd7a9e`.** The verdict was "minimal fix, no macro:
  stubs take the vector via `naked_asm!` `const` operand". What landed is a
  macro — `idt_vectors!` at `idt/mod.rs:188`, one table at `:237` where a
  vector's number, its stub's error-code form and its dispatch arm are declared
  together. The verdict's reasoning ("thirteen vectors do not amortize a DSL")
  was overtaken by the argument that all three had to agree and nothing made
  them; the agent who wrote it had not seen the note. Position: the note is
  answered, the "no macro" clause is not the tree's rule.
- **`syscall.rs`** — **OPEN, and it grew.** 2,061 lines at review, **2,248 on
  2026-08-08**. Target unchanged: arch entry asm + ABI register mapping stays
  in `arch/`; an arch-neutral `syscall::dispatch` with the user-pointer decode
  as one visible, auditable layer (where the cwd-accumulation and
  derived-allocation bug class lived); handlers in `kernel/src/syscall/` by
  subsystem (fs, ipc, shm, mm, process, dl), designed so handlers can grow host
  tests behind effect traits. ARM adds only an entry stub. Sequencing note:
  #133 (`SyscallError::Io`) has since landed on the old layout.

### drivers/

- **`acpi.rs`** — **OPEN.** Better than feared (typed `TableError`, named
  bounds, packed structs only for `offset_of!`). Target: host-tested crate on
  the gpt model — after the checksum walk proves every byte readable, the table
  becomes `&[u8]` into a zerocopy-typed layer, unsafe deletes, fixtures are
  real dumps (QEMU + T14). No `toyos-acpi` exists. This is the ACPI/AML track's
  foundation stage; an AML interpreter is the ultimate host-testable component.
- **`i8042/`** — **OPEN.** Not a rewrite (its shape is two field investigations
  of hard-won behaviour). One C-ism, confirmed: health state is `static HEALTH:
  AtomicU8` (`mod.rs:206`) over bare `HEALTH_*` `u8` constants, with
  `claim_health(from: u8, to: u8)` (`:324`) as its transition — an enum with a
  transition method is the Rust form. ThinkPad mentions: see doctrine §1.5 —
  the sweep is **much larger than the "~20 sites across six kernel files" this
  verdict recorded**; see the hardware-name entry in `specs/issues/design-debt/`.
- **`panic_console/`** — **AFFIRMED, no rewrite, and the owner's own question
  is what it answers.** It already seizes the scanout from whoever holds it
  (direct-map write, WC + sfence), arms before serial, never paints on
  recovery, paginates for a phone camera, and its GOP-only scope is a stated
  decision (virtio-gpu's scanout needs the lock/poll family a panic path must
  never join, and that config has serial). Obligations elsewhere: the
  current-scanout-descriptor seam when mode-setting arrives; the capture test
  (`specs/issues/panic-path/`, "Nothing distinguishes `panic_console::capture` from a
  no-op") remains the real gap.
- **`xhci/`** — **SUBSTANTIALLY DONE, `2e81ae8`.** The one targeted extraction
  — the **port state machine** behind a register-access trait with a host
  simulator — is built: `toyos-xhci/` is 2,082 lines over `port.rs` (414),
  `enumerate.rs` (372), `job.rs` (380), `portsc.rs` (292), `recovery.rs` (278),
  `protocol.rs` (246), `invariants.rs` (75). `specs/xhci-port-machine-plan.md`
  is the plan of record and its own §3 says X0 and X1 are on `main` (`6bfeed9`)
  with X2a and X2b built and X2c open. `xhci/mod.rs` is still 1,825 lines, so
  the effects shell has not shrunk to match. The auditor's F-K (`with_storage`
  safe only by a non-local invariant nothing asserts) was the design input it
  was meant to be.

### Comment policy instances

`bootloader/main.rs:164`, `bcachefs_adapter.rs:17` ("now runs the whole way" —
still present, verbatim), `log_file.rs` narration (46% of its 564 lines are
comment lines) — all fall to the one-time codebase-wide sweep under doctrine
§1.5. **OPEN**, and now filed with its measurements as `specs/issues/design-debt/` rather
than living only here, because five of the owner's 35 notes were this one
position and none of them had a home an agent would find.

### Extraction cluster

- **`symbols.rs`** (293 lines, `core`+`alloc` only) — **OPEN.** Extract; host
  tests with a real symbol blob. No `toyos-symbols` exists.
- **`elf.rs`** — **DONE, `e2c6a06` (crate) + `42b29c9` (kernel wired).** The
  parse half is `toyos-elf/` (1,604 lines, 56 host tests, crafted-input
  corpus); the lib cache and the mapping half stayed kernel-side as
  `kernel/src/elf/` (1,186 lines over four files).
- **`loader.rs`** — **PARTLY DONE, `42b29c9`.** Now `kernel/src/loader/` (1,397
  lines over four files). The pure decisions (segment overlap,
  vaddr-to-file-offset, TLS block arithmetic, relocation validation) are in
  `toyos-elf` and host-tested. What is **not** built is the plan/execute split:
  a pure layer computing the complete construction (mappings, protections,
  TLS/stack/guards, relocations, initial registers) as data,
  host-property-tested (no overlap, W^X, guards present), with a small executor
  applying it through a narrow address-space interface and the trampoline as
  the only per-arch piece. `loader/mod.rs` is 896 lines and still a sequence of
  effects.
- **`process.rs`** — **OPEN**, 1,743 lines. Target: the lifecycle
  (created/running/zombie/reaped, kill-in-transit, wait) joins the simulated
  core in `toyos-sched`'s discipline; capability-handles defines what a process
  *owns*, the state machine what it *is*. #142/#156 (`specs/issues/kernel/`) are the
  standing evidence that its bugs are interleaving bugs.
- **`main.rs` gates / `usb_gate.rs` / `input_merge_test.rs`** — **OPEN**,
  doctrine §1.6. `usb_gate` stays (hardware is the subject; a raw block device
  has no path to userland). `input_merge_test` signals trapped pure logic:
  extract the merge state machine (one held-set, button-merge, bounded queues),
  host-test with synthetic multi-source streams, gate shrinks or deletes.

### Subsystems and userland

- **Log subsystem** — **OPEN, and the owner's note was a question, not a
  decision.** Today: a macro file, `log_file.rs`'s flush, the ring, the panic
  drain, the screen capture — scattered, with known sins (unbounded
  uninterruptible flush in the idle loop, which is one of #156's two suspect
  prologues; userland `println!` sharing the ring; the ring as a wake
  condition). Target *if* the owner says go: a log core (ring + context
  stamping once) with sinks — serial, file, screen — as independent consumers
  with explicit backpressure: a slow sink drops-and-counts, never blocks, does
  no unbounded work in scheduler-adjacent paths, fails alone. Filed as an open
  question in `specs/issues/design-debt/` with the file inventory attached.
- **Kernel layout** — **OPEN, question for the owner.** 39 flat `.rs` files in
  `kernel/src/` beside seven subdirectories (`arch/`, `drivers/`, `elf/`,
  `iommu/`, `loader/`, `mm/`, `sched/`). Target: subsystem directories (fs/,
  ipc/, input/, proc/, log/, time/ — the syscall split already forces one).
  Same `specs/issues/design-debt/` entry as the log question, because the owner asked both
  in one note.
- **`block.rs:73`** — **OPEN, and this verdict was wrong.** It said the private
  `PAGE_SIZE = 4096` "dedups to mm's export". **`mm` exports no 4 KiB
  constant** — `mm/paging.rs` has only `PAGE_SIZE_BIT` (a PDE flag) and
  `mm/user_span.rs` only `PAGE_2M`. There are five private 4096s to reconcile
  and nothing yet to reconcile them to: `block.rs` `PAGE_SIZE`,
  `file_cache.rs:13` `PAGE_SIZE`, `file_backing.rs:9,10`
  `BLOCK_SIZE`/`BLOCK_SIZE_U64`, `fat32_adapter.rs:75` `BLOCK`, `usb_gate.rs:31`
  `BLOCK`. The cache-budget policy stays where it is (storage policy reading mm
  stats).
- **`mm/region.rs`** — **OPEN, already filed.** The file's own TODOs are the
  verdict: `KernelSlice::from_raw` lets any caller invent base and size in a
  type whose purpose is bounds. Covered in full by `specs/issues/design-debt/`,
  "`KernelSlice::from_raw` cannot check the one thing that makes the type
  safe", which carries the three call sites and the fix shape. Open question
  for the deep dive: whether `check`'s `offset + len` arithmetic holds under
  overflow in the shipped profile — not verified in session, deliberately not
  claimed, and still not verified.
- **`toyos/` SDK** — **OPEN.** Full API-surface audit: ownership-correct handle
  types (the `into_fd`/`mem::forget` dance at `toyos/src/lib.rs:120-124` is the
  symptom, still there), one error story, docs; aligned with capability-handles
  since that changes what a handle is. This crate must eventually survive
  crates.io and upstream reviewers. Overlaps `specs/issues/design-debt/`'s "`Fd` is a
  Unix-ism" and "`SharedToken` is a bare `u32` with no RAII", which are two
  instances of it.
- **`compositor` / `soundd`** — **compositor DONE (`763712b`, `72705d9`);
  soundd OPEN and it grew.** Doctrine §1.1 applied: the compositor's
  damage/layout/Z-order are `toyos-desktop/` (2,684 lines, pure, host-tested)
  and `userland/compositor/` is 2,085 lines of effects over five files with a
  68-line `main.rs`. soundd was 1,637 lines in one `main.rs` at review and is
  **1,924 on 2026-08-08**, with mixing and format conversion still inline and
  one `mod tests` at `:1892`; its pure core (sample-exact mixing, complementing
  gate A which keeps certifying the device end) is not extracted. For soundd
  this is what keeps the planned in-process HDA driver absorbable.
- **`console` arrow keys** — **PARKED, unchanged.** Arrows belong to the shell
  (history/line editing, which do not exist yet); PageUp/Down keep scrollback
  unchorded as designed, and `console/src/main.rs` still binds only those two
  (`:53-54`, `:222`, `:226`). Parked inside a future shell-ergonomics look.

## 3. Deep-dive agenda

Topics for later dedicated agents, each to be examined in detail and
scientifically — measurements, alternatives priced, a spec before code.
Order deliberately unassigned.

1. **OPEN** — the asm contract: PerCpu offsets, one source of truth. Vector
   numbers are done (`9bd7a9e`); the 47 `gs:[N]` sites are not.
2. **OPEN** — `syscall.rs` decomposition.
3. **DONE for the extraction** (`2e81ae8`) — the xHCI port machine and its host
   sim exist and `specs/xhci-port-machine-plan.md` carries the remaining
   stages. What is left is X2c and shrinking `xhci/mod.rs`.
4. **OPEN** — the log subsystem redesign (core + backpressured sinks). Gated on
   the owner: see §2.
5. **`toyos-elf` DONE** (`e2c6a06`); `toyos-symbols` extraction still open —
   `symbols.rs` and its `elf` crate dependency are what is left.
6. **OPEN** — `loader.rs` plan/execute split. Partly done (§2); what is not
   built is a `LoadPlan` value an executor applies, and M2 (#159) changes what
   a mapping's protection is, so the plan's shape is not settled yet.
7. **OPEN** — process-lifecycle host model (with capability-handles).
8. **OPEN** — `toyos-acpi` crate (= ACPI/AML track stage 0).
9. **OPEN** — the `toyos` SDK API audit.
10. **HALF DONE** — compositor split landed (`763712b`, `72705d9`); soundd's
    core/effects split has not started.
11. **OPEN** — the comment-policy sweep + hardware-name scrub + gates
    quarantine + kernel directory layout (one mechanical wave). Filed with
    measurements in `specs/issues/design-debt/`.
12. **OPEN** — small deletions from §2 (gdt shim, empty AP hook, debug tools,
    percpu items) — can ride any adjacent task. Filed in `specs/issues/design-debt/`.

## 4. The review channel

`jan-review` was a message channel, not code: notes were read as a diff against
main, converted to verdicts here and entries in `specs/issues/`, and never landed.
**The branch and its worktree are deleted** (2026-08-08) — a judgement reachable
only from a branch nobody reads is a judgement that does not exist, and the
notes had already begun to rot against files that moved underneath them
(`elf.rs`, `loader.rs`, `compositor/main.rs` and `user_ptr.rs`'s
`user_slice_of_mut` no longer exist in the shape they were annotated in).

§5 is the ledger that makes the branch redundant. If the owner wants a second
batch, a fresh branch off current main annotated the same way is the flow — and
it should be consumed within days, not weeks.

## 5. The note ledger

All 35 notes from `60855a4` and `d5b492e`, verbatim, with where each went.
"§2" means the verdict above; a hash means the commit that answered it.

| # | Site | Note | Destination |
|---|---|---|---|
| 1 | `bootloader/main.rs:164` | why so long comments? | §2 comment policy; `specs/issues/design-debt/` comment sweep |
| 2 | `arch/apic.rs` `init_timer_ap` | why? | §2 arch/ — OPEN, delete the ceremony |
| 3 | `arch/gdt.rs` | no empty files no lazy refactorings | §2 arch/ — OPEN, delete the shim |
| 4 | `arch/idt/mod.rs` `from_raw` | all of this mapping is brittle … can we use macros smarter and reduce code and complexity? | **ANSWERED `9bd7a9e`** (`idt_vectors!`) |
| 5 | `arch/mod.rs` | `#[allow(dead_code)]` on `debug` — why is that needed? | §2 arch/ — OPEN, five uncalled tools; `specs/issues/build/` + `specs/issues/design-debt/` |
| 6 | `arch/mtrr.rs` | what is this for? do we still need / want this? | §2 arch/ — answered, keep as-is |
| 7 | `arch/percpu.rs` `PerCpu` | are all of those fields needed? | §2 — OPEN, `lapic_id` is the one dead field |
| 8 | `arch/percpu.rs` asserts | verifies against constants but doesnt guarantee the constants are the same used in e.g. `preempt.rs` | **`specs/issues/design-debt/`** — 47 `gs:[N]` sites bound to nothing |
| 9 | `arch/percpu.rs` `alloc_percpu` | why not set all fields? do we have sentinels here? | §2 — OPEN, 8 of 22; sentinel is asm wire format and stays |
| 10 | `arch/syscall.rs` | way too big … full analysis / refactor / rewrite … more testable and less architecture specific | §2 arch/, deep dive 2 — OPEN, 2,248 lines |
| 11 | `bcachefs_adapter.rs:17` | "now runs the whole way" thats narration slop | `specs/issues/design-debt/` comment sweep — still present |
| 12 | `block.rs:73` | `PAGE_SIZE` belongs to the paging subsystem no? | §2 — OPEN; the recorded verdict was wrong, mm exports no such constant |
| 13 | `drivers/acpi.rs` | can we make this more compile time safe and use more object oriented type checked patterns? | §2 drivers/, deep dive 8 — OPEN, `toyos-acpi` |
| 14 | `drivers/i8042/mod.rs` | never mention ThinkPad in the kernel source code | **`specs/issues/design-debt/`** — 6 ThinkPad + 59 T14 sites in kernel `.rs` |
| 15 | `drivers/i8042/mod.rs` | pretty big … might include C isms | §2 drivers/ — one found: `HEALTH: AtomicU8` wants an enum |
| 16 | `drivers/panic_console/mod.rs` | this is our BSOD … no matter who claimed the gpu we take it … do you agree? does this need a rewrite? | §2 drivers/ — **agreed, no rewrite**; that is what it already does |
| 17 | `drivers/xhci/mod.rs` | really big. refactor / analysis / rewrite? | **SUBSTANTIALLY DONE `2e81ae8`**; `specs/xhci-port-machine-plan.md` |
| 18 | `elf.rs` | can maybe be moved into a crate and tested, is that true? | **ANSWERED — yes. `e2c6a06` + `42b29c9`** |
| 19 | `input_merge_test.rs` | what is this, does this belong here? | §1.6 — a cfg-gated actuator; target is `kernel/src/gates/` |
| 20 | `loader.rs` | splittable into a crate and testable? | **PARTLY `42b29c9`**; plan/execute split still owed |
| 21 | `log.rs` | should we redesign and rewrite the log subsystem and rethink the kernel's file/folder structure? | **`specs/issues/design-debt/` — OPEN QUESTION for the owner**, evidence attached |
| 22 | `log_file.rs` | theres slop narration in the comments | `specs/issues/design-debt/` comment sweep — 46% comment lines |
| 23 | `main.rs:3` | `#![allow(dead_code)]` why? | **FILED `451edff`** — `specs/issues/build/`, 49 hidden warnings |
| 24 | `main.rs:552` | those test gates … arent they a bit intrusive? … more use of host tests? | §1.6 — correctly gated, target is quarantine + extraction |
| 25 | `main.rs` (end) | the whole codebase has too many comments. good code speaks for itself | **`specs/issues/design-debt/`** — the consolidated entry |
| 26 | `mm/region.rs:12` | look at all TODOs | **already filed** — `specs/issues/design-debt/` `KernelSlice::from_raw`; still exactly two, tree-wide |
| 27 | `process.rs` | huge. crate? test? | §2, deep dive 7 — OPEN, 1,743 lines |
| 28 | `sched/driver.rs` | so many comments … refer to the spec and let the code speak? | `specs/issues/design-debt/` comment sweep |
| 29 | `symbols.rs` | crate material / host testable? | §2, deep dive 5 — OPEN, 293 lines |
| 30 | `usb_gate.rs` | what is this? | §1.6 — a cfg-gated actuator; stays, hardware is its subject |
| 31 | `user_ptr.rs:246` | `#[allow(dead_code)]` why? audit all places of this | **ANSWERED `4353289`** — `user_slice_of_mut` deleted; the audit is `specs/issues/build/` |
| 32 | `toyos/src/lib.rs` | audit this whole crate for a super clean and ergonomic api surface | §2, deep dive 9 — OPEN |
| 33 | `compositor/main.rs` | too big. rewrite / refactor, migrate to host tests? | **DONE `763712b` + `72705d9`** |
| 34 | `console/main.rs:214` | allow more keys like arrow | §2 — PARKED, arrows belong to a shell that does not exist |
| 35 | `soundd/main.rs` | too big, refactor / rewrite?, qemu tests to host tests migration? | §2 — OPEN, and it grew to 1,924 |
</content>
