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
| kernel | 389 (was 373 when this row was opened; re-measured 2026-08-20 — the earlier figure predated the `boot-actuators+test-actuators` clippy invocation `host-tests.yml` gained the same day, which alone accounts for 13 of the 16-block difference) | **measured in full; four areas adopted, four to go.** Per-module counts below. |
| userland | unmeasured | the `toyos` fork toolchain has no `cargo-clippy` component (`rustup component add` refuses a custom-linked toolchain; it would need building from `rust/` via `x.py`) — this can't be measured until that exists, independent of the lint-adoption question |

### Kernel, per module (measured 2026-08-20, union of both kernel clippy invocations)

| module | unsafe blocks | status |
|---|---:|---|
| `drivers/` | 121 | not yet swept |
| `arch/` | 107 | not yet swept |
| root files (`user_ptr.rs`, `process.rs`, `preempt.rs`, `inbox.rs`, `main.rs`, `symbols.rs`, `hw.rs`, `file_backing.rs`, `sync.rs`, `scheduler.rs`, `pipe.rs`, `page_cache.rs`, `bcachefs_adapter.rs`) | 76 | not yet swept |
| `mm/` | 35 | **adopted** — every site now carries a `SAFETY:` comment; two (`object::shm::Pages`, `mm::paging::AddressSpace`'s `Send`/`Sync` impls) turned out to look vestigial rather than load-bearing, filed rather than removed: `issues/kernel/redundant-send-sync-impls-mm-object.md` |
| `elf/` | 23 | **adopted** — every site now carries a `SAFETY:` comment; writing the justification found two functions (`elf::read_backing_into`, `elf::index::RelocationIndex::apply_to_page`) that write through a raw pointer without being `unsafe fn`, filed as `issues/kernel/raw-pointer-writers-not-marked-unsafe-in-loader.md` (every current call site is correct; the gap is that nothing enforces the next one being) |
| `sched/` | 8 | not yet swept |
| `loader/` | 8 | **adopted** — every site now carries a `SAFETY:` comment (one finding shared with `elf/`, above) |
| `iommu/` | 8 | not yet swept |
| `log/` | 2 | not yet swept |
| `object/` | 1 | **adopted** — the one site is `object::shm::Pages`'s `Send`/`Sync` pair, part of the finding filed above |
| `completion/` | 0 undocumented (3 unsafe sites, all already carrying a `SAFETY:`/`Safety:` comment predating this pass) | already documented |
| **adopted this pass** | **67** (`mm` 35 + `elf` 23 + `loader` 8 + `object` 1) | gated: each area's entry module (`mm/mod.rs`, `object/mod.rs`, `elf/mod.rs`, `loader/mod.rs`) opens with `#![warn(clippy::undocumented_unsafe_blocks)]`, which composes with `host-tests.yml`'s existing `-D warnings` on both kernel invocations — no command-line change needed, since the kernel is one crate with no `-p` scoping and the module attribute is what stands in for it |
| **remaining** | **322** (`drivers` 121 + `arch` 107 + root files 76 + `sched` 8 + `iommu` 8 + `log` 2) | measured, not gated — follow-up |

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
validity requirement is real but not type-enforced — filed as
`issues/kernel/raw-pointer-writers-not-marked-unsafe-in-loader.md`. Four
real findings from three areas, which is the whole reason this pass writes
the justification by hand instead of pattern-matching the comment shape.

## Shape (unchanged from stage one)

**`clippy::restriction` is never enabled as a group.** Cherry-picked only —
`undocumented_unsafe_blocks` above is the current example.

Turning `pedantic`/`nursery` on wholesale has one predictable outcome — a
blanket `allow` to make CI pass — which is worse than not turning them on.
That is still the reason this stays a per-lint decision rather than a group
switch, six lints in.
