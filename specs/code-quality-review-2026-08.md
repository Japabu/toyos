# Code-quality review, 2026-08 — verdicts and target states

The owner reviewed the tree and left 34 `// REVIEW:` notes (branch `jan-review`,
commits `60855a4` and `d5b492e`). Every note was walked with him in session on
2026-08-05/06 and every verdict below was agreed. **Nothing here is scheduled
work.** This records the destination — where the code should end up, decided
while explicitly refusing to plan the route — so later deep-dive agents (§3)
start from settled ground instead of relitigating it. Effort was not an
argument in any verdict.

## 1. The doctrine

Seven commitments, each already proven somewhere in the tree and now adopted as
the target everywhere.

1. **Decisions are pure and host-tested; the kernel keeps only effects.** The
   full ladder: unrepresentable > compile-checked > host-tested > guest-tested,
   with a guest boot reserved for what genuinely lives at the hardware
   boundary. Host tests are preferred because they are faster — the owner's
   explicit constraint.
2. **Every format is a crate.** `toyos-gpt`, `toyos-ps2`, `toyos-fat32`,
   `toyos-keymap` exist; `elf` (parse half), `symbols`, `acpi` join; the ladder
   ends at bcachefs. Fixtures are real artifacts; fuzzing the trust boundary
   happens on the host where it is cheap.
3. **Every state machine has a host model.** The scheduler has one
   (`toyos-sched`); process lifecycle, the xHCI port machine, and the USB BOT
   transport join it. The 2026-08-05 T14 session's three worst bugs (retire
   race, SS-port wedge, spawn wedge) were all interleaving or state-space bugs
   in unmodeled machines.
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
   call — never compiled into an ordinary build), but they live interleaved
   with production sources; target is one directory (e.g. `kernel/src/gates/`)
   so what test machinery exists is auditable in one listing. An actuator must
   name whether its subject is hardware (stays) or pure logic hiding behind
   hardware (extract the logic, host-test it, shrink or delete the gate).
7. **A TODO in code is either a filed task or a fix.** Two TODOs in
   `mm/region.rs` sat invisible until this review tripped over them.

Consequence worth stating: CLAUDE.md's "minimal kernel" claim, currently
false (the kernel holds two filesystems, four virtio drivers, xHCI, NVMe,
i8042, GOP, a page cache, io_uring), becomes structurally true as logic
migrates to crates and models — the kernel trends toward an effects layer.

## 2. Verdicts, by area

File:line references are as of `jan-review` (`d5b492e`, based on `d5174a5`).

### arch/

- **`gdt.rs`** — a re-export shim with one caller (`syscall.rs:80`). Delete;
  the caller imports `percpu::KERNEL_CS`.
- **`apic.rs` `init_timer_ap()`** — empty, one caller, correct behaviour
  (calibration is one global measurement; `arm_one_shot` programs divide/LVT
  per call). Delete the ceremony. May legitimately return for heterogeneous
  ARM cores; that day reintroduces it.
- **`arch/mod.rs` `allow(dead_code)` on `debug`** — masks five uncalled
  investigation tools (`set_context`, `watch_write`, `clear`, `monitor_pte`,
  `check_pte_monitor`). Delete the five (git history is the shelf), keep
  `read_dr6` + `context` (called from the #DB handler), drop the allow. Folds
  into the crate-level allow(dead_code) task (#37).
- **`mtrr.rs`** — keep as-is, no new comment. It is the read-only decoder
  behind the one log line that diagnoses a mis-typed scanout on any future
  machine; the owner's question was a knowledge gap, not a code defect.
- **`percpu.rs` offsets** — the const asserts pin `offset_of!` to literals
  while `preempt.rs`, the syscall entry, `idt/mod.rs`, `timer.rs`, `tlb.rs`,
  `device_irq.rs` and the `log!` macro carry twenty-plus hand-written
  `gs:[N]` copies bound to nothing. Target: `offset_of!` fed into asm as
  `const` operands — one source, asserts delete. **Must precede any PerCpu
  field surgery.**
- **`percpu.rs` fields** — all consumed except `lapic_id` (zero readers
  outside the file); drop it after the offset unification. The syscall
  rip/num/rbp trio stays: three stores per entry buy every panic the ability
  to name where userland was.
- **`percpu.rs` init** — partial init relying on a zeroed allocation becomes
  one total `ptr::write(PerCpu { .. })`. The tid/pid `u32::MAX` sentinel
  stays: asm wire format, already `Option`-decoded at the boundary.
- **`idt/`** — a vector number lives in four coordinated places (stub push
  literal, dispatch match, `from_raw`, table slot). Minimal fix, no macro:
  stubs take the vector via `naked_asm!` `const` operand; `trap_dispatch`
  decodes once through `from_raw`. Thirteen vectors do not amortize a DSL,
  and the stub/dispatch layer is per-architecture anyway.
- **`syscall.rs`** — 2,061 lines holding three things. Target: arch entry asm
  + ABI register mapping stays in `arch/`; an arch-neutral
  `syscall::dispatch` with the user-pointer decode as one visible, auditable
  layer (where the cwd-accumulation and derived-allocation bug class lived);
  handlers in `kernel/src/syscall/` by subsystem (fs, ipc, shm, mm, process,
  dl), designed so handlers can grow host tests behind effect traits. ARM
  adds only an entry stub. Sequencing note: #133 (`SyscallError::Io`) rides
  the new layout rather than churning the old.

### drivers/

- **`acpi.rs`** — better than feared (typed `TableError`, named bounds,
  packed structs only for `offset_of!`). Target: host-tested crate on the
  gpt model — after the checksum walk proves every byte readable, the table
  becomes `&[u8]` into a zerocopy-typed layer, unsafe deletes, fixtures are
  real dumps (QEMU + T14). This is the ACPI/AML track's (#136) foundation
  stage; an AML interpreter is the ultimate host-testable component.
- **`i8042/`** — not a rewrite (its shape is two field investigations of
  hard-won behaviour). One C-ism: health/claim state as raw `u8` wants an
  enum. Deep audit folds into #56. ThinkPad mentions: see doctrine §1.5 —
  the sweep covers ~20 sites across six kernel files.
- **`panic_console/`** — design affirmed, no rewrite. It already seizes the
  scanout from whoever holds it (direct-map write, WC + sfence), arms before
  serial, never paints on recovery, paginates for a phone camera, and its
  GOP-only scope is a stated decision (virtio-gpu's scanout needs the
  lock/poll family a panic path must never join, and that config has
  serial). Obligations elsewhere: #135 gains the current-scanout-descriptor
  seam when mode-setting arrives; #44 (capture test) remains the real gap.
- **`xhci/`** — not a rewrite; one targeted extraction: the **port state
  machine** behind a register-access trait with a host simulator (doctrine
  §1.3). This is the vehicle for #151 (SS ports get the USB2 hot-reset,
  Inactive needs a warm reset the driver lacks, protocol capability never
  parsed), #152 (full blocking enumeration under the ticket lock inside the
  scheduler pass), and #156's prologue half. The auditor's F-K (`with_storage`
  safe only by a non-local invariant nothing asserts) is a design input: the
  extraction makes that invariant representable. End state per doctrine
  §1.6: the BOT/SCSI protocol becomes a host-modeled machine and `usb_gate`
  certifies only the DMA/register boundary.

### Comment policy instances

`bootloader/main.rs:164`, `bcachefs_adapter.rs:17` ("now runs the whole
way"), `log_file.rs` narration — all fall to the one-time codebase-wide sweep
under doctrine §1.5. CLAUDE.md's three comment paragraphs compress to one
rule during #74, which also gains the no-Python rule.

### Extraction cluster

- **`symbols.rs`** (270 lines, `core`+`alloc` only) — extract; host tests
  with a real symbol blob.
- **`elf.rs`** — **done.** The parse half is `toyos-elf/`; the lib cache and the
  mapping half stayed kernel-side as `kernel/src/elf/`.
- **`loader.rs`** — target is a **plan/execute split**: a pure layer computes
  the complete construction (mappings, protections, TLS/stack/guards,
  relocations, initial registers) as data, host-property-tested (no overlap,
  W^X, guards present); a small executor applies it through a narrow
  address-space interface; the trampoline is the only per-arch piece.
- **`process.rs`** — target: the lifecycle (created/running/zombie/reaped,
  kill-in-transit, wait) joins the simulated core in `toyos-sched`'s
  discipline; capability-handles defines what a process *owns*, the state
  machine what it *is*. #142/#156 are the standing evidence that its bugs
  are interleaving bugs.
- **`main.rs:552` gates / `usb_gate.rs` / `input_merge_test.rs`** — doctrine
  §1.6. `usb_gate` stays (hardware is the subject; a raw block device has no
  path to userland). `input_merge_test` signals trapped pure logic: extract
  the merge state machine (one held-set, button-merge, bounded queues),
  host-test with synthetic multi-source streams, gate shrinks or deletes.

### Subsystems and userland

- **Log subsystem** — the strongest redesign candidate. Today: a macro file,
  `log_file.rs`'s flush, the ring, the panic drain, the screen capture —
  scattered, with known sins (unbounded uninterruptible flush in the idle
  loop, which is one of #156's two suspect prologues; userland `println!`
  sharing the ring; the ring as a wake condition). Target: a log core (ring
  + context stamping once) with sinks — serial, file, screen — as
  independent consumers with explicit backpressure: a slow sink
  drops-and-counts, never blocks, does no unbounded work in
  scheduler-adjacent paths, fails alone.
- **Kernel layout** — ~40 flat files in `kernel/src/`. Target: subsystem
  directories (fs/, ipc/, input/, proc/, log/, time/ — the syscall split
  already forces one).
- **`block.rs:69`** — the private `PAGE_SIZE = 4096` dedups to mm's export;
  the cache-budget policy stays where it is (storage policy reading mm
  stats).
- **`mm/region.rs`** — the file's own TODOs are the verdict:
  `KernelSlice::from_raw` lets any caller invent base and size in a type
  whose purpose is bounds. Target: constructors only from proven provenance
  (a `PhysPage`, the validated initrd extent, a checked direct-map range);
  `from_raw` deletes. Open question for the deep dive: whether `check`'s
  `offset + len` arithmetic holds under overflow in the shipped profile —
  not verified in session, deliberately not claimed.
- **`toyos/` SDK** — full API-surface audit: ownership-correct handle types
  (the `into_fd`/`mem::forget` dance is the symptom), one error story, docs;
  aligned with capability-handles since that changes what a handle is. This
  crate must eventually survive crates.io and upstream reviewers.
- **`compositor` (2,560 lines) / `soundd` (1,637)** — doctrine §1.1 applied:
  damage/layout/Z-order and mixing/format-conversion respectively are pure
  cores, host-tested (compositor: "damage covers every change" as a
  property; soundd: sample-exact mixing, complementing gate A which keeps
  certifying the device end). Effects shells stay thin. For soundd this is
  what keeps the planned in-process HDA driver absorbable.
- **`console` arrow keys** — arrows belong to the shell (history/line
  editing, which do not exist yet); PageUp/Down keep scrollback unchorded as
  designed. Parked inside a future shell-ergonomics look.

## 3. Deep-dive agenda

Topics for later dedicated agents, each to be examined in detail and
scientifically — measurements, alternatives priced, a spec before code.
Order deliberately unassigned.

1. The asm contract: PerCpu offsets + vector numbers, one source of truth.
2. `syscall.rs` decomposition (with #133 riding it).
3. The xHCI port-state-machine extraction + host sim (vehicle for
   #151/#152/#156-prologue; F-K as design input).
4. The log subsystem redesign (core + backpressured sinks).
5. **`toyos-elf` done**; `toyos-symbols` extraction still open. The parse half
   is a crate with 56 host tests and a crafted-input corpus; `symbols.rs` and
   its `elf` crate dependency are what is left.
6. `loader.rs` plan/execute split. **Partly done**: the pure decisions (segment
   overlap, vaddr-to-file-offset, the TLS block's arithmetic, relocation
   validation) are in `toyos-elf` and host-tested, and `spawn` is 220 lines of
   named steps rather than 613 of inline arithmetic. What is *not* built is a
   `LoadPlan` value an executor applies — the mapping half is still a sequence
   of effects, and M2 (#159) is about to change what a mapping's protection is,
   so the plan's shape is not settled yet.
7. Process-lifecycle host model (with capability-handles).
8. `toyos-acpi` crate (= ACPI/AML track stage 0).
9. The `toyos` SDK API audit.
10. Compositor and soundd core/effects splits.
11. The comment-policy sweep + ThinkPad name scrub + TODO reconciliation +
    gates quarantine + kernel directory layout (one mechanical wave).
12. Small deletions from §2 (gdt shim, empty AP hook, debug tools, percpu
    items) — can ride any adjacent task.

## 4. The review channel

`jan-review` in the owner's worktree (`../toyos-review`) is a message
channel, not code: notes are read as a diff against main, converted to
verdicts here and tasks in the tracker, and never land. Batch 1+2 of notes
(all 34) are consumed by this spec; the owner can reset the branch to
current main and annotate fresh code, and future batches repeat the flow.
