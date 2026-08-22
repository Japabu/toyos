---
status: open
kind: defect
opened: 2026-08-19
---

# Stage two: `pedantic`/`nursery`, one lint at a time, plus `undocumented_unsafe_blocks` per area

**The owner ruled on 2026-08-20, and it governs every remaining area sweep:
reduction before documentation.** Each `unsafe` block's first question is
whether it can stop existing — a safe abstraction, an existing type, a
restructure — and the work of removing it is worth doing, not dodged for a
comment. Only what proves irreducible gets the SAFETY comment, and that
comment states *why* it is irreducible, not just why it is sound. An area's
report counts blocks REMOVED beside blocks documented; a sweep that removed
nothing explains why nothing was removable. The first sweep's own best
findings — two removable `unsafe impl`s discovered by trying to justify
them — are the precedent: the already-swept areas' filed reduction findings
execute under this ruling too.

Stage one (#132) put default clippy on every PR — kernel, host workspace,
bootloader, `-D warnings`. This entry now carries stage two: every
`pedantic`/`nursery` lint measured on all three trees, adopted or rejected
by name with the count that decided it, plus `undocumented_unsafe_blocks`
measured per area. Method: never a group, never a guess — a lint's findings
were read, not just counted, before it was let anywhere near the gate.

Measuring the full `pedantic` + `nursery` groups (`-W` only, not gated) across
the host workspace, kernel and bootloader found **~9,000 findings** across
~90 distinct lints (up from the 1,684-on-the-kernel-alone figure this entry
carried before — that number was kernel only; the host workspace, including
`toyos-ld` and `toyos-cc`, carries most of the rest). Six lints earned a
place in the gate. Everything else stayed off, each for a reason below.

## Adopted

Each fixed at every site it fired, then added to all four `clippy` step
invocations in `host-tests.yml` as an individual `-W clippy::<lint>` (never
the group).

| lint | count | why |
|---|---:|---|
| `checked_conversions` | 1 | `kernel/src/arch/syscall.rs`'s device-register-write syscall bounded a `u64` against `u32::MAX` with a manual `as` comparison, then cast twice more in the body. Restructured through `u32::try_from` — one conversion, no repeated casts, at a trust-boundary site that is exactly what this bar is for. |
| `manual_midpoint` | 3 | `(a + b) / 2` can overflow near the integer's max; `T::midpoint` can't. Two in a `toyos-desktop` test, one in `toyos-sched/sim`'s binary-search shrinker — no realistic overflow today, but the fix is free and closes the class everywhere, forever. |
| `redundant_clone` | 6 | Real, not the false-positive-prone lint its `nursery` placement suggested — every instance checked was a clone of a binding never used again after (verified by hand, not by trusting the lint): `kernel/src/arch/syscall.rs`, a `kernel-loom` test, `toyos-cc`'s parser (a double clone — the match scrutinee was already an owned value), and three in `tests/toyos.rs`. |
| `unchecked_time_subtraction` | 1 | `tests/toyos.rs`: a bare `Duration - Duration` that panics identically today either way — `.checked_sub().expect(msg)` says why it can't underflow instead of hiding the same panic behind an operator. No behavior change; an honesty win at zero cost. |
| `unnecessary_semicolon` | 2 | Two dead `;` after `if` statements in `kernel/src/process.rs` and `kernel/src/main.rs`. Nothing to weigh. |
| `default_trait_access` | 1 | `bootloader/src/main.rs`'s UEFI `open()` call passed `Default::default()` where `FileAttribute::default()` names the type. One site, one import. |

`undocumented_unsafe_blocks` — see its own section below; bootloader (10) and
`toyos-abi` (19) are adopted the same way, gated as their own invocations.

## Rejected, measured

Every count below is `pedantic` + `nursery` together, workspace + kernel +
bootloader, read (not just counted) before the call. The large-count rows are
exactly what this entry predicted before landing stage one: `must_use_candidate`,
`cast_possible_truncation` and kin fire on nearly every line of a kernel and
are almost always deliberate. The smaller rows earned their own look; four of
them turned out to be real findings this pass chose not to fix, not false
positives — `volatile_composites`, `suboptimal_flops`/`imprecise_flops`, and
the two filed below as their own issues.

| lint | count | character |
|---|---:|---|
| `cast_possible_truncation` | 1,594 | deliberate width-narrowing throughout drivers/build tooling |
| `use_self` | 1,255 | style — `Self` vs. the type's own name |
| `cast_lossless` | 1,024 | `as` vs `From` for already-safe widening casts, no behavior change |
| `must_use_candidate` | 892 | mechanical `#[must_use]` annotation |
| `missing_const_for_fn` | 853 | huge mechanical cost, negligible benefit for functions nothing evaluates at compile time |
| `doc_markdown` | 823 | backtick-wrapping identifiers in prose |
| `cast_possible_wrap` | 557 | same character as `cast_possible_truncation` |
| `missing_errors_doc` / `missing_panics_doc` | 313 / 133 | doc-section mechanical churn |
| `redundant_pub_crate` | 261 | style |
| `cast_sign_loss` / `cast_precision_loss` | 217 / 114 | same cast-lint character |
| `redundant_closure_for_method_calls` | 177 | style |
| `too_long_first_doc_paragraph` | 130 | doc style |
| `too_many_lines` | 117 | this tree writes long functions on purpose (see `the-comment-density-position.md`) |
| `option_if_let_else` / `map_unwrap_or` | 115 / 101 | style |
| `uninlined_format_args` | 105 | style |
| `match_same_arms` | 101 | style, sometimes clearer split |
| `items_after_statements` | 97 | style |
| `unreadable_literal` | 89 | style |
| `ptr_as_ptr` / `ref_as_ptr` / `borrow_as_ptr` / `ptr_cast_constness` | 69 / 34 / 25 / 18 | mechanical pointer-cast respelling, no behavior change, ~150 combined sites — the driver/MMU code's `as`-cast idiom is pervasive and deliberate |
| `manual_let_else` | 63 | style |
| `cast_ptr_alignment` | 63 (57 kernel) | **checked**: the kernel instances are the MMIO byte-pointer-to-register-width-pointer idiom — alignment is guaranteed by the hardware register layout the datasheet defines, not provable from the Rust type. Same character as the cast lints above. |
| `single_match_else` | 62 | style |
| `similar_names` | 50 | **checked**: fires on `ecx1`/`edx1` and `ebx7`/`ecx7` from CPUID leaf destructuring — the naming convention *is* the register names; not confusing to a systems reader |
| `unused_self` | 44 | **checked, false positive on this tree's idiom**: `serial::BackendGuard::has_data`/`try_read_byte` (and the same shape in `mm::paging::child_mut`, xHCI device setup) don't read `self`'s fields, but holding `&self`/`&mut self` *is* the capability — it proves the caller holds the lock/exclusive access the method needs. Converting to associated functions would delete that proof. |
| `needless_pass_by_ref_mut` | 28 | **checked, same false-positive class as `unused_self`** — every kernel instance is a `&mut` enforcing exclusive access to memory reached through a raw pointer (a DMA buffer, a page table, `BackendGuard`'s lock), not a literal field mutation the lint's heuristic can see |
| `case_sensitive_file_extension_comparisons` | 28 | **checked, false positive**: every instance compares a file extension this project's own deterministic build tooling produced (`.o`, `.bin`), never external/untrusted input |
| `suboptimal_flops` / `imprecise_flops` | 37 / 2 | all in `src/wallpaper.rs`'s procedural image generator — a floating-point rounding change is a visual-output change, which CLAUDE.md requires the owner's sign-off for; out of scope here |
| `duration_suboptimal_units` | 37 | cosmetic — smaller-unit `Duration` constructors, no behavior change |
| everything else (~50 more lints, 1–30 each) | ~450 combined | style/perf-neutral suggestions (`format_push_string`, `bool_to_int_with_if`, `wildcard_imports`, `assigning_clones`, `self_only_used_in_recursion`, `zero_sized_map_values`, …) — sampled, not individually fixed; none changes correctness, dead code, or a name's honesty |

Three rejections worth spelling out because they looked like real bugs on
first read and turned out not to be, which is the whole point of reading
before adopting:

- **`suspicious_operation_groupings` (8 findings, 2 distinct sites)**:
  `toyos-gpt/src/lib.rs`'s three-clause partition-bounds check
  (`partition.first_lba > partition.last_lba || partition.first_lba <
  header.first_usable_lba || partition.last_lba > header.last_usable_lba`)
  and `toyos-ld/src/emit_macho.rs`'s Mach-O header-size sum both look like
  copy-paste bugs to the lint's cross-clause heuristic and are correct as
  written — verified against what each is actually checking, not assumed.
- **`large_stack_arrays` / `large_stack_frames` (4 findings)**: flagged a
  `kernel::log::shard::Shard` construction that "may allocate 1048648 bytes
  on the stack." `Shard::new`, `Rendered::EMPTY` and `TraceRing::new` are
  every one of them `const fn`s used *only* to initialize `static`s — the
  lint can't see that the array never touches a stack at all, since it's
  const-evaluated into `.data`/`.bss`. The fourth (a `toyos-fat32` host test)
  is a 64 KiB local array on a host thread with megabytes of stack.
- **`non_send_fields_in_send_ty` (3 findings)**: `NvmeBlockDevice`,
  `XhciController`, `Pipe` all manually `unsafe impl Send` over a
  raw-pointer-owning driver struct — the architecture's standard shape for
  "this device's memory has one owner, enforced by the driver, not by the
  type." Two of three already carry a `# Safety`/`SAFETY:` comment saying so.

## `undocumented_unsafe_blocks`, per area

| area | unsafe blocks | status |
|---|---:|---|
| bootloader | 10 | **adopted** — every site now carries a `SAFETY:` comment; gated |
| `toyos-abi` | 19 | **adopted** — every site now carries a `SAFETY:` comment; gated as its own `-p toyos-abi` invocation, not folded into the workspace one (the workspace command also lints ~20 other host-workspace crates whose unsafe blocks nobody has reviewed yet — enabling the lint there would silently gate crates this pass never looked at) |
| kernel | 389 (was 373 when this row was opened; re-measured 2026-08-20 — the earlier figure predated the `boot-actuators+test-actuators` clippy invocation `host-tests.yml` gained the same day, which alone accounts for 13 of the 16-block difference) | **measured in full; five areas adopted, five to go.** Per-module counts below. |
| userland | unmeasured | the `toyos` fork toolchain has no `cargo-clippy` component (`rustup component add` refuses a custom-linked toolchain; it would need building from `rust/` via `x.py`) — this can't be measured until that exists, independent of the lint-adoption question |

### Kernel, per module (measured 2026-08-20, union of both kernel clippy invocations)

| module | unsafe blocks | status |
|---|---:|---|
| `drivers/` | 121 | **adopted 2026-08-22 — the first sweep under the reduction ruling.** 121 undocumented blocks (132 `unsafe` blocks in all); **60 removed, 72 documented, 1 filed.** Per driver below — and the filed one was built the same day, taking the area from 72 blocks to 37. |
| `arch/` | 107 | **adopted 2026-08-22 — the last area, and the highest-risk one.** 107 findings (112 `unsafe` blocks in all); **30 removed, 77 documented, 3 filed.** Per file below. |
| root files (`user_ptr.rs`, `process.rs`, `preempt.rs`, `inbox.rs`, `main.rs`, `symbols.rs`, `hw.rs`, `file_backing.rs`, `sync.rs`, `scheduler.rs`, `pipe.rs`, `page_cache.rs`, `bcachefs_adapter.rs`) | 76 | **adopted** — 13 removed, 63 documented, 4 filed. Per file and per finding below. |
| `mm/` | 35 | **adopted** — every site now carries a `SAFETY:` comment; two (`object::shm::Pages`, `mm::paging::AddressSpace`'s `Send`/`Sync` impls) turned out to look vestigial rather than load-bearing, filed rather than removed: `issues/kernel/redundant-send-sync-impls-mm-object.md` |
| `elf/` | 23 | **adopted** — every site now carries a `SAFETY:` comment; writing the justification found two functions (`elf::read_backing_into`, `elf::index::RelocationIndex::apply_to_page`) that write through a raw pointer without being `unsafe fn`, filed rather than fixed. Both take a `mm::KernelSlice` now (2026-08-22), so the extent is the allocation's and the file is closed. |
| `sched/` | 8 | **adopted 2026-08-22**, under the reduction ruling. 8 findings (26 `unsafe` blocks in all); **4 removed, 5 documented, 1 filed** — the five sites of the same shape outside this area, since taken by the root-file and `arch/` sweeps, so that file is closed too. |
| `loader/` | 8 | **adopted** — every site now carries a `SAFETY:` comment (one finding shared with `elf/`, above) |
| `iommu/` | 8 | **adopted 2026-08-22**, under the reduction ruling. 8 findings (8 `unsafe` blocks in all); **5 removed, 3 documented, 0 filed** — the area's whole remainder is two window constructions and a `clflush`. |
| `log/` | 2 | **adopted 2026-08-22**, under the reduction ruling. 2 findings (9 `unsafe` blocks in all, seven of them documented before this pass); **0 removed, 2 documented, 1 filed** — and the filed one was built the same day: `LogRecord` has the safe `as_bytes` its six siblings carry, so `log::user`'s hand-rolled slice is gone and the area's remainder is one block (`shard::initialize_zeroed`, irreducible). |
| `object/` | 1 | **adopted** — the one site is `object::shm::Pages`'s `Send`/`Sync` pair, part of the finding filed above |
| `completion/` | 0 undocumented (3 unsafe sites, all already carrying a `SAFETY:`/`Safety:` comment predating this pass) | already documented |
| **adopted** | **389** — the whole kernel (`mm` 35 + `elf` 23 + `loader` 8 + `object` 1 + root files 76 + `drivers` 121 + `arch` 107 + `sched` 8 + `iommu` 8 + `log` 2) | gated at the source, because the kernel is one crate with no `-p` scoping to hang a lint on. Every area sweep opened its entry module (`mm/mod.rs`, `object/mod.rs`, `elf/mod.rs`, `loader/mod.rs`, `drivers/mod.rs`, `arch/mod.rs`, `sched/mod.rs`, `iommu/mod.rs`, `log/mod.rs`) with `#![warn(clippy::undocumented_unsafe_blocks)]`; the root-file sweep could not — there is no entry module above them but the crate root — so it **inverted the form**: `main.rs` carries one crate-level `#![warn(...)]`, and an `#[allow(...)]` on a `mod` line is the form a tree not yet swept would take. Both compose with `host-tests.yml`'s existing `-D warnings` on the two kernel invocations, so no command line changed. The module attributes are redundant under the crate one and are left where they are — each still records its own area's status. |
| **remaining** | **0** | measured 2026-08-22, after the last two sweeps (`arch`, and `sched`+`iommu`+`log`) merged, with `--force-warn clippy::undocumented_unsafe_blocks` over both kernel invocations: no finding anywhere in `kernel/src`. The `allow` list in `main.rs` is empty, which is the state this row was opened to reach; a new tree that cannot be gated the day it appears goes on that list and comes off it by the pull request that sweeps it. |

### `drivers/`, per driver (swept 2026-08-22)

Total `unsafe` blocks before and after, counted over
`kernel/src/drivers/**/*.rs` excluding comment lines; "found" is the
`undocumented_unsafe_blocks` finding count, which is smaller where a driver
already had some documented blocks.

| driver | found | blocks before | after | removed |
|---|---:|---:|---:|---:|
| `xhci/` (6 files) | 36 | 38 | 17 | 21 |
| `panic_console/mod.rs` | 18 | 18 | 17 | 1 |
| `nvme.rs` | 13 | 13 | 9 | 4 |
| `virtio.rs` | 13 | 13 | 3 | 10 |
| `virtio_gpu.rs` | 11 | 11 | 2 | 9 |
| `acpi.rs` | 9 | 9 | 2 | 7 |
| `virtio_console.rs` | 9 | 9 | 5 | 4 |
| `virtio_sound.rs` | 2 | 7 | 7 | 0 |
| `hda.rs` | 2 | 5 | 5 | 0 |
| `serial.rs` | 4 | 5 | 3 | 2 |
| `mod.rs` | 2 | 2 | 1 | 1 |
| `virtio_net.rs` | 2 | 2 | 1 | 1 |
| **total** | **121** | **132** | **72** | **60** |

**Then 37, the same day.** The one reduction this sweep filed rather than did
was built immediately after it: `mm::Dma`, a safe pool-borrowing view in two
disciplines, which deleted every one of the 35 blocks that were "a bounds-checked
view over `DmaPool` memory" plus the two `unsafe impl Send`s that only existed
because a struct held raw pointers into a pool. Counted the same way, over both
`cfg` arms: **74 before, 37 after**. (74 rather than 72 because that counter also
counts `serial::panic_flush`'s `unsafe fn` signature and `msc`'s
`stack-witness`-gated block.) What is left is exactly the "not this" list below:
`panic_console` 17, `virtio_sound` 6, `serial` 4, `hda` 3, `virtio_console` 3,
`acpi` 2, `xhci/wait/mod.rs` 1, `xhci/wait/msc.rs` 1.

Five abstractions did most of it, and none is a wrapper for the lint's
sake — each replaces a raw-pointer expression whose bound was the *offset*
with one whose bound is the offset *and the length*:

- `acpi::Mapped` — a firmware-supplied physical address `table_at` has
  bounded, with the module's only two dereferences on it. Writing the
  justification found that `xsdt` read four fields off an RSDP address that
  had never been through `table_at`, so the two cases `MAX_PHYS` exists to
  stop reached `as_ptr` and wrapped; routed through the check now.
- `virtio::Ring` — the `Mmio` of a virtqueue. Eleven inline
  `read_volatile(slice.ptr_at(off) as *const T)` became three methods.
- `serial::SavedFlags` — an `RFLAGS` word that came out of `pushfq`, which
  is what makes `restore` a safe method; `DF` set is the failure this type
  makes unrepresentable.
- `xhci`'s five helpers — `zero_dma` (12 sites), `write_dcbaa` (3),
  `TrbRing::put` (3), `msc::read_dma` (4), `msc::write_dma` (2).
- `virtio_gpu::{put, answer}` — the driver's one writer and one reader of
  DMA memory, which also deleted three `core::slice::from_raw_parts` views
  built only to feed `copy_nonoverlapping`.

Four `unsafe impl`s were tested for redundancy by replacing each with a
`fn _p<T: Send>()` probe and compiling: `DmaPool`'s was redundant and is
deleted, and `NvmeBlockDevice`, `VConsole`, `ConsoleCell`, `GpuController`,
`VirtioNic`, `FbCell` and `RenderedCell` all failed the probe and were
kept. Three more were deleted a different way — by giving `VConsole`,
`GpuController` and `VirtioNic` `KernelSlice` fields instead of `*mut u8`,
after which the auto trait applies.

**Two real holes closed while writing the justifications**, both of the
same kind — a bound that was on the offset and not on the length:

- `xhci::device::read_back` parsed a configuration descriptor through
  `core::slice::from_raw_parts(buf, delivered as usize)`, where `delivered`
  is a length the *device* chose in its Transfer Event. A device reporting
  more than the 256-byte scratch page held read past it.
- `acpi::xsdt`'s unchecked RSDP address, above.

**What was not removable, and why.** `panic_console` gave up one block of
eighteen: its five `UnsafeCell` statics cannot be `Lock`s because the panic
path may take no synchronisation primitive (`Lock::lock` panics after 500M
spins; `try_lock` can dispatch the scheduler), and `Fb` at 40 bytes and
`Rendered` at `SNAPSHOT_CAP` bytes fit no atomic. Its framebuffer accesses
reach a `*mut u8` from the GOP descriptor, which no `mm` type describes.
The two audio drivers gave up none: restructuring either changes what a
device does, which root `CLAUDE.md` puts behind the owner's sign-off.

**One reduction filed rather than done**, because it was one abstraction
across every driver rather than a change inside one: 35 of the 72 blocks that
remained were the same shape — a bounds-checked view over `DmaPool` memory
reached through a `KernelSlice` whose every accessor is an `unsafe fn` — and the
five driver-local helpers this sweep wrote were five approximations of the one
thing that belonged on `DmaPool`. It is `kernel/src/mm/dma.rs` now; that module's
header is where the two disciplines and the lifetime are argued.

### `arch/`, per file (swept 2026-08-22)

The largest unswept area left and the highest-risk one — `entry.rs` is every
Ring 0 entry, `syscall.rs` is the user/kernel boundary, and `control_regs.rs`
is the one declaration every CPU applies. 107 findings reproduced exactly
(`--force-warn clippy::undocumented_unsafe_blocks` over both kernel
invocations). `unsafe` blocks before and after are counted over
`kernel/src/arch/**/*.rs` excluding comment lines; "found" is smaller than
"before" wherever a file already had documented blocks or blocks behind a
feature neither invocation enables.

| file | found | blocks before | after | removed |
|---|---:|---:|---:|---:|
| `percpu.rs` | 30 | 32 | 26 | 6 |
| `cpu.rs` | 21 | 23 | 23 | 0 |
| `idt/exceptions.rs` | 12 | 12 | 11 | 1 |
| `smp.rs` | 11 | 11 | 5 | 6 |
| `syscall.rs` | 9 | 10 | 10 | 0 |
| `idt/mod.rs` | 7 | 7 | 2 | 5 |
| `mod.rs` | 4 | 4 | 4 | 0 |
| `control_regs.rs` | 4 | 4 | 4 | 0 |
| `apic.rs` | 4 | 4 | 1 | 3 |
| `fpu.rs` | 3 | 3 | 3 | 0 |
| `pat.rs` | 1 | 1 | 1 | 0 |
| `debug.rs` | 1 | 1 | 1 | 0 |
| **total** | **107** | **112** | **91** | **21** |

**`entry.rs` is not in that table and neither is `tlb.rs`, `mtrr.rs` or any of
the ten small `idt/` handlers: they contain no `unsafe` block at all.** The
bracket and the entry stubs are `naked_asm!`, which is a whole function and not
a block, so the file that owns the `cld` every Ring 0 entry clears was never
this lint's subject and is untouched by this sweep.

**30 findings removed against 21 blocks, and the difference is nine new
documented blocks** — eight `const`-generic GS primitives in `arch::percpu::gs`
and one inside `smp.rs`'s `asm_label_addr!`. Two more removals landed outside
the area: `preempt.rs` gave up all six of its blocks to the same move, and
`mm/paging.rs` gained one.

**`percpu.rs` reached its own fields three ways and now reaches them one.**
`preempt.rs` had six `const`-generic GS primitives and three hand-copied offset
literals (240, 244, 256) while `arch/percpu.rs` — which declares `PerCpu`,
asserts every offset against the number the entry stubs hardcode, and reaches
the same fields — used a raw `asm!` string per accessor *and* `(*percpu_ptr()).field`.
The primitives move to `arch::percpu::gs`, gain `read_u64` and a register-form
`write_u8`, and every GS access in the kernel goes through them; `preempt.rs`
keeps its policy, loses its assembly, and its literals become
`offset_of!`-derived constants that are still asserted against 240/244/256.

**What the `(*percpu_ptr()).field` form actually cost, which is why this was a
reduction and not a respelling.** `[profile.toyos]` sets
`debug-assertions = true`, so every one of those dereferences carried a null
test, an alignment test and two panic landing pads —
`panic_null_pointer_dereference` and `panic_misaligned_pointer_dereference` —
reached from `trap_dispatch`, `crash_report`, `fatal_exception`,
`sched::driver::execute` and `Machine::idle_wait`. Panic landing pads on the
crash path. `movq %gs:0,%rsi; testb $7,%sil; jne; testq; je; movq 216(%rsi),%r14`
is now `movq %gs:216, %r14`, and the kernel's 27 `gs:[0]` self-pointer loads
are 5.

**Three `asm!("cli"/"sti")` in `idt/mod.rs`** became
`arch::cpu::{enable,disable}_interrupts`, which is that instruction with those
options — three of the five the `sched/` sweep filed. The other two were in
`main.rs` and the root-file sweep had already taken them, so nothing of that
finding was left.

**`smp.rs`'s `asm_label_addr!` took its `unsafe` inside.** A `lea` off `rip`
computes an address and reads nothing, so there was no caller obligation to
state and the six sites that wrote `unsafe { asm_label_addr!(…) }` were each
restating the same nothing. **`pat.rs` and `smp.rs` also stopped spelling
instructions `arch::cpu` already names** — `mov cr0`, `mov cr4`, `wbinvd`,
`mov cr3` — and `mm::paging::load_kernel_flush` is `activate_kernel`'s sibling
for the one caller that reaches a CR3 switch before `CR4.PCIDE` is set on its
CPU, where `activate`'s `NOFLUSH` bit is reserved.

**`cpu.rs` gave up nothing and gained a module header instead.** Its
twenty-one blocks are all irreducible the same way — one instruction Rust has
no operation for, at the bottom of the tree, with no lower layer to push it
into — and saying that twenty-one times says nothing. The header says it once;
each site adds the instruction's own failure mode and which half of it the
signature discharges. Writing that is what found the first of the three filings
below.

**Three findings, filed rather than fixed. All three are closed** — the six
safe-and-should-not-be wrappers are four `unsafe fn` and two whose signatures
carry the argument, the `#DB` handler is deleted with `debug_trap` gating what
replaced it, and `sys_query_modules`'s `transmute_copy` is gone: `ModuleInfo`
carries the safe `as_bytes` its siblings do, with the `const _` that says the
two trailing `u32`s leave no tail padding for kernel stack bytes to hide in
(all on 2026-08-22).

`syscall.rs`'s two raw writes into `PageAlloc` memory are a third site of the
shape `issues/kernel/pagealloc-has-no-checked-window.md` already carries, and
the TLS one has its bound fifty lines and one function away from the write;
both say so at the site rather than opening a second file about it.

**The oracle was the compiler, and the numbers say what moved.** Emitting the
kernel's assembly at the guest's own `[profile.toyos]` with both actuator
features on gives **216,782 instruction lines before and 216,584 after**.
Thirteen functions differ and three disappear (`percpu::syscall_num`,
`idle_stack_top` and `set_fault_state` were out-of-line calls and are one
instruction each now); every one of the differences is a per-CPU field read
losing its pointer, its debug-assertion checks and its landing pads, or the
register allocation downstream of that. **No privileged or string instruction
changed count anywhere in the kernel**: `cld` 12, `sti` 14, `cli` 37, `wrmsr`
58, `rdmsr` 19, `wbinvd` 3, `fxsave64` 12, `fxrstor64` 13, `invlpg` 3, `cpuid`
41, `iretq` 14, `sysretq` 1, `lgdt` 4 — and `IA32_FMASK`'s `$263680` appears
twice in both. The naked entry stubs are instruction-identical. The instrument
has teeth: mutating one `enable_interrupts()` to `disable_interrupts()` moved
exactly one line of the 216,584, `sti` → `cli`.

### The root files, per file (swept 2026-08-22)

The first sweep run under the owner's ruling at the top of this file, and the
counts are what the ruling asks for: removed beside documented.

| file | blocks | removed | documented | how the removal was done |
|---|---:|---:|---:|---|
| `user_ptr.rs` | 23 | 2 | 21 | the two pointer computations in `window()` became `wrapping_add`. Not a spelling change: both exist to be *compared* against a translation, testing whether the next page is still the same physical allocation — so on the run this function exists to catch, `add` is undefined behaviour and the comparison it feeds is one the optimizer may fold. Neither result is ever dereferenced. |
| `process.rs` | 14 | 5 | 9 | four `unsafe { kernel_cr3().activate() }` became `paging::activate_kernel()`, a **safe** function (new, in `mm/paging.rs`, one documented block): `Cr3::activate` is unsafe because an arbitrary `Cr3` may name freed tables, and the kernel's own never can. Two raw writes in `write_argv` became one `UserStack::write_at`, which bounds the **whole** write where `kern_ptr` bounded only its first byte. |
| `preempt.rs` | 9 | 3 | 6 | nine hand-written `asm!` strings became six `const`-generic primitives (`read_u32`, `write_u32`, `read_u8`, `write_u8`, `lock_inc_u32`, `lock_dec_u32`). The offset is a `const` operand, so the emitted instruction is unchanged — verified against `--emit asm`: `lock addl $1, %gs:240`, `movb $1, %gs:244`, `movl %gs:240, %eax`, immediate displacements throughout. |
| `inbox.rs` | 7 | 0 | 7 | nothing removable: the ABI *is* a byte layout at fixed offsets in a shared page, so a typed view has to be minted. What the justification found instead is filed (below). |
| `main.rs` | 6 | 2 | 4 | two raw `asm!("cli")` became `arch::cpu::disable_interrupts()`, which is that exact instruction with those exact options and already existed. |
| `symbols.rs` | 4 | 0 | 4 | two raw-pointer tables shared with `toyos-elf` (which forbids `unsafe`), a `Sync` impl that was the second half of an already-commented pair, and the leaked `AtomicPtr` a panic reads lock-free. |
| `hw.rs` | 3 | 0 | 3 | `IrqGuard`'s `pushfq`/`cli` pair and `sti; hlt`. Irreducible by *sequence*: saving `IF` and clearing it must be one uninterruptible run, and `sti` must precede `hlt` with no boundary between. |
| `file_backing.rs` | 3 | 0 | 3 | the initrd's `Send`/`Sync` pair and its extent-to-pointer path. The justification found a real gap — filed. |
| `sync.rs` | 2 | 0 | 2 | `LockGuard`'s `Deref`/`DerefMut`. Turning a `&UnsafeCell<T>` into `&T` is what a lock is. |
| `scheduler.rs` | 2 | 0 | 2 | the futex word's read, and `IdleProof::new_unchecked`. The proof token is itself a reduction already taken — `reap_finished` needs no `unsafe` because of it. |
| `pipe.rs` | 1 | 0 | 1 | `unsafe impl Send for Pipe`. **Checked rather than assumed**: deleting it fails to compile (`*mut u8 cannot be sent between threads safely`) — the test the two vestigial `mm`/`object` pairs failed. |
| `page_cache.rs` | 1 | 1 | 0 | `alloc_zeroed` + `Box::from_raw` for a 1 MiB chunk became `vec![0u8; CHUNK_SIZE].into_boxed_slice()`, which reaches the same `alloc_zeroed` through `alloc`'s zeroing specialization for `u8` with no stack temporary. The field type went from `Box<[u8; CHUNK_SIZE]>` to `Box<[u8]>`; every use of a chunk was already a `[off..off + 4096]` slice. |
| `bcachefs_adapter.rs` | 1 | 0 | 1 | `SliceBlockIO::new` over the initrd region from `KernelArgs`. |
| **total** | **76** | **13** | **63** | |

Four findings came out of writing the justifications, filed rather than fixed
because each was a decision past a documentation sweep. **All four were decided
and fixed on 2026-08-22**, which is what the sweep's own report predicted would
be worth more than the comments were:

- Six of `inbox.rs`'s seven blocks minted `&`/`&mut` over a page the process
  maps writable, which is the shape `user_ptr.rs`'s `UserBytes` header argues
  against; three were `&mut`, and `create` mapped the page into the process
  *before* building the headers. **Decided and fixed**: the page is built before
  it is mapped and `SharedMemObject::phys_before_mapping` refuses the reversed
  order; the ring headers are reached one `&AtomicU32` at a time and a
  submission is a `read_volatile` of the whole entry, so no reference over that
  page exists at all.
- `InitrdBacking` was given the *file's* size and never the image's, so nothing
  bounded an extent against the end of the initrd it came out of. **Decided and
  fixed 2026-08-22**: it holds the image (`bcachefs::SliceBlockIO`, now `Copy`)
  and reads through `SliceBlockIO::block`, which is the one thing that knows
  both the block number and the length. `file_backing.rs` and
  `bcachefs_adapter.rs` now contain no `unsafe` at all — the `Send`/`Sync` pair,
  the extent-to-pointer path and the copy out of it are gone, and the one
  remaining claim about the initrd region is `SliceBlockIO::new`'s, made once in
  `main.rs` where the region is named. A reduction, not a comment.
- The futex word and the crash dump read user memory with `*ptr` where
  `copy_in` uses `read_volatile`. **Decided and fixed 2026-08-22**: both are
  `read_volatile`, and the rule is now `user_ptr.rs`'s module header rather
  than a per-site preference.
- The demand-paging fill's two raw writes wanted a bounded window type, and the
  obvious `&mut [u8]` is the borrow that is wrong for pages about to become a
  user mapping. **Decided and fixed 2026-08-22**: no new type — `PageAlloc`
  gained a safe `window()` that hands back the `mm::KernelSlice` this kernel
  already has, sized from the allocation it owns, and `UserStack` holds one
  instead of its own private bound. That was also the first third of the
  "allocators construct the slice" fix that `KernelSlice::from_raw` was open on;
  `DmaPool` took the second (it is `mm::Dma` now) and the ELF loader the third,
  and with all three done `from_raw` is deleted and that file is closed.

One correctness fix landed with the sweep rather than being filed, because
writing the comment is what found it and the fix is a deleted token:
`preempt.rs`'s `disable`/`enable`/`enable_no_resched` declared
`options(preserves_flags)` on `lock add`/`lock sub`, which write OF, SF, ZF,
AF, CF and PF. The claim was false, so the compiler was entitled to keep a
comparison's result live across a preempt-count change. No other `lock`-prefixed
`asm!` in the kernel makes that claim — `arch/`'s entry stubs are `naked_asm!`
with no options at all.

Documenting `toyos-abi/src/ring.rs`'s nine unsafe blocks surfaced a real
open question about whether its `&[u8]`/`&mut [u8]` views alias a page
userland can also write — filed as
`issues/build/ring-rs-shared-slice-over-a-userland-writable-page.md` rather
than guessed at here. Measuring `volatile_composites` (a `restriction` lint,
cherry-picked, not part of this pass's scope but caught while reading kernel
driver code) found 10 genuine sites — filed as
`issues/kernel/volatile-composites-on-mmio-dma-structs.md`, not fixed, because
the fix means re-deriving each device's own field-ordering contract (xHCI's
Cycle-bit-last rule among them) and getting it wrong silently is worse than
today's implementation-defined-but-tested state.

Documenting `mm/` and `object/` surfaced two more `unsafe impl Send`/`Sync`
pairs that `cargo check` says are unnecessary — `object::shm::Pages` and
`mm::paging::AddressSpace` — filed together as
`issues/kernel/redundant-send-sync-impls-mm-object.md` rather than removed
here, the same reasoning as the two findings above: this pass's job was
documentation, and the fix touches a security-relevant module's actual code,
not its comments. Documenting `elf/` surfaced a related but distinct pattern
— two functions that take a raw pointer without being `unsafe fn`, so the
validity requirement is real but not type-enforced — filed rather than fixed,
and both take a `mm::KernelSlice` now. Four real findings from three areas,
which is the whole reason this pass writes the justification by hand instead
of pattern-matching the comment shape.

### `sched/`, `iommu/` and `log/`, the three small areas (swept 2026-08-22)

18 findings between them, which reproduced the 8/8/2 this table recorded on
2026-08-20 exactly. `unsafe` blocks before and after, counted over each area's
`.rs` files excluding comment lines; "found" is the `undocumented_unsafe_blocks`
finding count, smaller than "before" wherever an area already had documented
blocks.

| area | found | blocks before | after | removed |
|---|---:|---:|---:|---:|
| `sched/` (`driver.rs` 7, `dump.rs` 1) | 8 | 26 | 22 | 4 |
| `iommu/` (`vtd/table.rs` 5, `vtd/fault.rs` 3) | 8 | 8 | 3 | 5 |
| `log/` (`user.rs` 1, `shard.rs` 1) | 2 | 9 | 9 | 0 |
| **total** | **18** | **43** | **34** | **9** |

**`sched/` removed four by naming a function that already existed.**
`arch::cpu::{enable_interrupts, disable_interrupts}` are safe, `#[inline]`,
and expand to the same `asm!("sti"/"cli", options(nomem, nostack))` the four
sites spelled by hand — `driver::execute`'s idle arm (which may not use an
`IrqGuard`, because both its exits have to *set* `IF` rather than restore it)
and `dump::deaf_window`. Five more of the same shape were outside this sweep's
areas and were filed rather than reached into; the root-file and `arch/` sweeps
took all five, so nothing of that finding is left.

**`iommu/` removed five with one addition to `mm`.** `Mmio` was already a
bounds-checked volatile window with safe accessors and one constructor,
`paging::map_mmio`; `Mmio::over_phys` is a second, `unsafe`, for physical
memory this kernel owns and has no mapping left to do — and with it two raw
address derivations replace nine raw dereferences:

- `vtd::table::Table::window` — the 4 KiB of a remapping table, so an index, a
  byte offset and a 32-bit field are all bounded against the same 4 KiB rather
  than each against an `assert!` of its own. It also bounds the `clflush`
  operand, which is an address carrying no length: `Table::slot` hands the
  flush a subregion that has already been checked.
- `vtd::fault::window` — the register window of an armed unit, rebuilt inside
  the DMA-fault handler from the physical address `arm` published. That handler
  may take no lock, so what it can keep is an `AtomicU64` and not an `Mmio`;
  before this it dereferenced that address raw, and the bound on its fault-record
  walk lived in `Records::fit` at *arm* time, one function and one boot phase
  away from the read. `FaultUnit.regs` now holds the physical base (0 stays the
  unused-slot sentinel, because `vtd::window` refuses a register base of 0) and
  `FaultUnit.records` the offset inside the window it was declared as.

What stayed: `table::flush`'s `clflush`/`mfence`, which has no safe spelling
and no narrower one — the instruction takes an address and no length, so the
bound has to be on the caller's side, and it now is.

**`log/` removed nothing in the sweep itself, and both reasons are named at the
site.** `shard::initialize_zeroed` writes one word in place through a
caller-supplied pointer because the alternative is materialising a 512 KiB
`Shard` on the BSP's 16 KiB stack — irreducible. `log::user`'s record-to-bytes
slice was reducible and needed a sysroot claim to reduce, so the sweep filed it
and the next pull request that was claiming the sysroot anyway took it:
`LogRecord::as_bytes` exists, `log::user`'s block is gone, and the area is down
to the one irreducible site.

**What was reducible and was declined, with the reason.** The kernel stack
canary's two accesses (`write_stack_canary`, `check_stack_canary`) can go
through `OwnedAlloc::slice` → `KernelSlice`, which would bound them — but every
`KernelSlice` accessor is itself an `unsafe fn`, so the trade is one block for
another *plus* an `assert!`, and the reader runs inside `with_cpu`'s exclusive
region on every scheduler pass. Zero blocks removed for a per-pass cost is not
a reduction; both sites say so.

The scheduler substitution's oracle was the compiler. Emitting the kernel's
assembly at the guest's own `[profile.toyos]` (`opt-level = 2`) before and
after, and diffing instructions only, gave **0 lines of difference across
225,014** — `#[inline]` leaves nothing behind at that optimisation level, so
the four sites are the same four instructions they were. The instrument has
teeth: mutating one `enable_interrupts()` to `disable_interrupts()` moved
exactly one of those 225,014 lines, `sti` → `cli`.

## Shape (unchanged from stage one)

**`clippy::restriction` is never enabled as a group.** Cherry-picked only —
`undocumented_unsafe_blocks` above is the current example.

Turning `pedantic`/`nursery` on wholesale has one predictable outcome — a
blanket `allow` to make CI pass — which is worse than not turning them on.
That is still the reason this stays a per-lint decision rather than a group
switch, six lints in.
