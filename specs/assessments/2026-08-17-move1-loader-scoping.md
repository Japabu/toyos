# Move 1 — the userland loader: scoping, 2026-08-17

Dated evidence, frozen. The question asked: **does
`specs/plans/kernel-slimming-roadmap.md`'s Move 1 — taking relocation, symbol
resolution, TLS layout and the `dlopen` family out of Ring 0 — earn a spec, and
if so, when?**

This document produces no design and no spec. It answers the three questions
`specs/README.md`'s *"Before a spec exists"* section requires of a proposed
permanent concept, plus the two the owner added for this scoping: what is in
Ring 0 today, and what the work costs. Move 1 is a deletion rather than a new
concept, so the three questions are asked of the *current* arrangement: what
property does it fail to give, which of its bounds are over quantities it does
not set, and what does it assert that it could not.

## Provenance

Read-only audit, one agent, worktree `wt/toyos-move1`.

**Baseline: `af1b52d`**, the tip of `main` when the document was written
(`d11c198` at the start; the branch was fast-forwarded to `af1b52d` before the
measurements were re-taken, and every line number below resolves at `af1b52d`).

Line counts are `wc -l` on tracked `.rs` files. Where a range is cited it is a
line range in the file named, and the arithmetic was run rather than done by
hand. ELF measurements of real artifacts were taken with a throwaway host
program in `/tmp` built on `toyos-elf` itself — the tree's own decoder, no
external tool, no host binary beyond `cargo` — reading binaries out of the
primary checkout's build trees. Those artifacts were built on 2026-08-15
(`userland/`, `toyos-cc`, `toyos-ld`, the test cdylibs) and 2026-08-02
(`rust/build/x86_64-unknown-toyos/stage2`), so they are the tree's own output
at those dates and not necessarily byte-identical to what a build today would
produce.

The `rust/` submodule is not checked out in a worktree, so every measurement of
the fork estate — the `grep`s over `rust/library`, the reads of
`sys/pal/toyos/tls.rs` and `symbolize/gimli/libs_toyos.rs`, and the
`stage2/lib/*.so` artifacts — was taken read-only in the primary checkout, at
submodule `87971e6`, which is the commit this branch pins.

**Not verified here**: nothing was booted. No guest run was made, so every
statement about spawn *timing* is absent rather than estimated. Where a figure
below is an estimate it says so in the sentence that carries it.

---

## 0. The finding that reframes every other section

**No program this system ships is dynamically linked, and no `.so` is present
in any image a boot uses.**

Every binary in `system.toml`'s `[programs]`, plus `toyos-ld` and `toyos-cc`,
read with `toyos-elf`:

```
20 binaries examined, 0 with any dynamic-linking relocation or DT_NEEDED
```

Per binary, the numbers behind that:

| binary | file size | PT_LOAD | DT_NEEDED | RELATIVE | GLOB_DAT/JUMP_SLOT | TPOFF | DTPMOD/DTPOFF | `.dynsym` | `.gnu.hash` | `.symtab`+`.strtab` | PT_TLS memsz |
|---|---|---|---|---|---|---|---|---|---|---|---|
| `init` | 1,931,432 | 3 | 0 | 2,916 | 0 | 0 | 0 | 0 | 0 | 493,088 | 144 |
| `shell` | 1,852,120 | 3 | 0 | 2,833 | 0 | 0 | 0 | 0 | 0 | 450,640 | 144 |
| `logd` | 1,810,992 | 3 | 0 | 2,795 | 0 | 0 | 0 | 0 | 0 | 450,468 | 144 |
| `toybox` | 2,233,064 | 3 | 0 | 3,593 | 0 | 0 | 0 | 0 | 0 | 610,397 | 144 |
| `compositor` | 5,142,080 | 3 | 0 | 9,149 | 0 | 0 | 0 | 0 | 0 | 1,270,707 | 144 |
| `sshd` | 10,363,848 | 3 | 0 | 12,405 | 0 | 0 | 0 | 0 | 0 | 2,953,531 | 560 |
| `toyos-cc` | 15,016,248 | 3 | 0 | 17,533 | 0 | 0 | 0 | 0 | 0 | 4,382,380 | 152 |

The whole shipped system's use of the loader is: parse a file header, insert
three demand-paged regions, precompute between 2,795 and 17,533 `RELATIVE`
writes, allocate a TLS block of 144–560 bytes for one module, and read up to
4.4 MB of `.symtab`/`.strtab` so backtraces have names.

Consequently, on every boot this system performs:

- zero symbol resolutions,
- zero `GLOB_DAT`/`JUMP_SLOT` bindings,
- zero TLS relocations of any kind,
- zero library loads,
- zero `dlopen`s.

`kernel/src/elf/reloc.rs` (288 lines), `kernel/src/elf/cache.rs` (266),
`load_shared_lib` (`kernel/src/elf/mod.rs:265-438`), `load_needed_libs` and
`map_libs` (`kernel/src/loader/mod.rs:714-816`), `apply_tls_relocs` and
`exe_tpoff` (`:823-904`), `symbols::dynamic_map`/`static_map`/`read_symtab`
(`kernel/src/loader/symbols.rs:22-75`), `sys_dlopen`, `sys_dlsym` and
`sys_tls_alloc_block` are **dead on every shipped boot**. They are reached by
exactly two things: the four test cdylibs, and the crafted-ELF corpus.

```
$ grep -rn "crate-type" --include=Cargo.toml . | grep -v '^./rust/'
tests/toyos-rust-tests/tls-dlopen-lib/Cargo.toml:7:crate-type = ["cdylib"]
tests/toyos-rust-tests/tls-lib/Cargo.toml:7:crate-type = ["cdylib"]
tests/toyos-rust-tests/tls-multi-crate/Cargo.toml:7:crate-type = ["cdylib"]
tests/toyos-rust-tests/tls-cranelift/Cargo.toml:7:crate-type = ["cdylib"]
```

`src/build.rs:1328`'s `build_toyos_bins` — the only place a cdylib is built and
placed in an image — is called from `tests/common/qemu.rs:1535` and from nowhere
else. `.so` files reach test images and no other kind.

This does not make Move 1 wrong. It changes what Move 1 *is*: not the removal
of a hot path, but the removal of a large body of privileged code that the
shipping system never executes and that only a future workload — the hosted
rustc — will.

---

## 1. What is in Ring 0 today, measured

### 1.1 The files

```
$ wc -l kernel/src/loader/*.rs kernel/src/elf/*.rs kernel/src/symbols.rs
     963 kernel/src/loader/mod.rs
     335 kernel/src/loader/start.rs
     165 kernel/src/loader/symbols.rs
     207 kernel/src/loader/tls.rs
     266 kernel/src/elf/cache.rs
     164 kernel/src/elf/index.rs
     464 kernel/src/elf/mod.rs
     288 kernel/src/elf/reloc.rs
     336 kernel/src/symbols.rs
    3188 total
$ cat toyos-elf/src/*.rs | wc -l          →  1682
```

Plus four syscall bodies in `kernel/src/arch/syscall.rs`, measured by finding
the first `^}` after each `fn`:

| body | lines | count |
|---|---|---|
| `sys_dlopen` | 2559–2695 | 137 |
| `sys_tls_alloc_block` + `tls_alloc_block` | 2709–2779 | 71 |
| `sys_dlsym` | 2781–2792 | 12 |
| `sys_query_modules` | 2911–2972 | 62 |
| | | **282** |

**Program loading in Ring 0 is 4,870 + 282 = 5,152 lines**, against a kernel of
49,655 tracked `.rs` lines (`git ls-files kernel | grep '\.rs$' | xargs wc -l`)
— 10.4 %.

### 1.2 What leaves and what stays

Move 1's end state is "the kernel maps the `PT_LOAD`s of a static-PIE image and
jumps". Classified by function against that line:

**Leaves Ring 0 — 2,177 kernel lines**

| site | lines |
|---|---|
| `loader/mod.rs:99-115` `read_elf_table` | 17 |
| `loader/mod.rs:199-331` `ExeTables`, `file_off`, `table`, `read_exe_tables` | 133 |
| `loader/mod.rs:333-382` `exe_sym_count`, `rela_dyn_from_sections` | 50 |
| `loader/mod.rs:443-518` `spawn`: tables, relocation index, libraries, binding | 76 |
| `loader/mod.rs:542-598` `spawn`: TLS template, layout, relocations, block | 57 |
| `loader/mod.rs:705-904` `load_needed_libs`, `map_libs`, TLS relocations | 200 |
| `loader/tls.rs:1-207` the whole TLS builder | 207 |
| `loader/symbols.rs:22-75` `dynamic_map`, `static_map`, `read_symtab` | 54 |
| `elf/mod.rs:52-228` `LibMemory`, `LoadedLib`, `dlsym`, `ModuleImage` | 177 |
| `elf/mod.rs:259-464` `load_shared_lib` and its helpers | 206 |
| `elf/cache.rs` whole | 266 |
| `elf/index.rs` whole | 164 |
| `elf/reloc.rs` whole | 288 |
| the four syscall bodies above | 282 |

**Stays in Ring 0 — 1,271 kernel lines**

| site | lines |
|---|---|
| `loader/mod.rs:1-98` module header, `read_file_range` | 98 |
| `loader/mod.rs:116-198` `image_fits_user_half`, `insert_elf_regions` | 83 |
| `loader/mod.rs:383-442` `spawn`: open, header, layout, user-half check | 60 |
| `loader/mod.rs:519-541` `spawn`: user stack | 23 |
| `loader/mod.rs:599-704` `spawn`: symbols, kernel stack, table insert, enqueue | 106 |
| `loader/mod.rs:905-963` `INIT_PATH`, `spawn_init` | 59 |
| `loader/start.rs` trampolines, handles, argv, kernel stack | 335 |
| `loader/symbols.rs:76-165` `MAX_SYMBOL_BYTES`, `read_backtrace_table` | 90 |
| `elf/mod.rs:1-51` module header, `parse_layout` | 51 |
| `elf/mod.rs:229-258` `read_backing_into` | 30 |
| `symbols.rs` the panic-path symbol table (Move 3's subject, not Move 1's) | 336 |

(22 lines are module-header prose not attributed to either column;
1,895 + 1,271 + 22 + 282 = 3,470, which is the group's 3,188 plus the syscall
bodies.)

`unsafe` occurrences, `grep -c 'unsafe'` per file and per range:

| | occurrences |
|---|---|
| leaves | 29 (`tls.rs` 6, `elf/mod.rs` 9, `reloc.rs` 9, `cache.rs` 3, `index.rs` 2) |
| stays | 14 (`start.rs` 5, `symbols.rs` 7, `loader/symbols.rs:149` 1, `elf/mod.rs:245` 1) |

`toyos-elf` itself is `#![forbid(unsafe_code)]` and splits the same way: 600
lines (`dynamic.rs` 130, `gnu_hash.rs` 135, `rela.rs` 251, `tls.rs` 84) leave the
kernel's link entirely, 1,082 (`header.rs` 164, `layout.rs` 387, `lib.rs` 221,
`section.rs` 109, `sym.rs` 201) stay because the kernel still parses a file
header, a program header table, a section header table and a symbol table.

**The residual is the point.** `read_backtrace_table`
(`loader/symbols.rs:101`) is called unconditionally at `loader/mod.rs:600` for
every spawn, walks the section header table and reads up to
`MAX_SYMBOL_BYTES` = 16,777,216 bytes of `.symtab`/`.strtab` into kernel pages
the process owns. It is what lets a crash report name a user frame. **Move 1 as
the roadmap states it does not remove ELF parsing from Ring 0** — it removes
relocation, symbol *binding* and TLS. Whether kernel-side user backtraces
survive is a question the spec would have to answer explicitly; the roadmap does
not.

### 1.3 The syscall surface is five, not three

The roadmap names `SYS_DLOPEN` (55), `SYS_DLSYM` (56) and `SYS_DLCLOSE` (57).
Two more read the same state and cannot survive it leaving:

- **`SYS_TLS_ALLOC_BLOCK` (88)** — `syscall.rs:2716` resolves `module_id`
  against `data.elf.tls_modules`, which the loader built. With TLS layout in
  Ring 3 there is no kernel module list to resolve against.
- **`SYS_QUERY_MODULES` (91)** — `syscall.rs:2911-2972` builds its answer
  entirely out of `ElfInfo`: `elf_base`, `exe_vaddr_max`,
  `exe_eh_frame_hdr_*`, `loaded_libs`, `lib_paths`. Its one consumer is the
  unwinder (`rust/library/backtrace/src/symbolize/gimli/libs_toyos.rs`). With
  the module list in Ring 3, userland already knows the answer.

And one signature changes rather than retiring: **`SYS_THREAD_SPAWN` (40)**.
`process.rs:997-1040` builds every new thread's TLS block from
`data.elf.tls_modules` and hands the thread its `fs_base` through
`scheduler::enqueue_new`. If TLS layout is userland's, `fs_base` has to come
from the caller.

So Move 1's ABI surface is five retirements and one changed signature. That is
three times what the roadmap states, and each of them is an ABI change that
`CLAUDE.md` requires to land on its own PR first.

### 1.4 "The largest untrusted-input parser left in Ring 0" — refuted as written

Ring 0 untrusted-input families, `cat … | wc -l` over the files that decode
input the kernel did not produce:

| family | Ring 0 lines |
|---|---|
| filesystem (`bcachefs/src` 2,873 + `toyos-fat32/src` 3,005 + `toyos-gpt/src` 671 + `fat32_adapter.rs` 1,003 + `bcachefs_adapter.rs` 544 + `gpt.rs` 323) | **8,419** |
| USB / xHCI (`drivers/xhci/**` 5,670 + `toyos-xhci/src` 2,117) | **7,787** |
| **program loading** (`loader/` 1,670 + `elf/` 1,182 + `symbols.rs` 336 + `toyos-elf/src` 1,682 + 282 syscall) | **5,152** |
| HDA (`drivers/hda.rs` 766 + `toyos-hda/src` 2,960) | 3,726 |
| virtio + net | 2,087 |
| PS/2 (`drivers/i8042/**` + `toyos-ps2/src`) | 2,065 |
| ACPI + DMAR | 1,041 |

**Program loading is third, not first.** The roadmap's own Move 2 calls the
filesystem "the second-largest untrusted parser in Ring 0"; the measurement
puts it first, 63 % larger than program loading.

The claim is nevertheless defensible on two narrower axes, and both are worth
stating because they are what actually justify the move:

1. **Reachability.** A crafted ELF is a file any process writes and hands
   straight to the kernel. `kernel/src/tmpfs.rs:14` records why `/tmp` is
   backed at all: *"Without this nothing under /tmp was spawnable or
   dlopenable"*, and `tests/toyos-rust-tests/src/bin/abuse_elf_loader.rs` is 507
   lines of exactly that attack, run from an ordinary unprivileged guest
   program. The filesystem parsers need a block device: an image the build
   produced, the boot disk, or a USB stick somebody plugged in. Program loading
   is the only large parser in Ring 0 whose input an unprivileged in-guest
   process composes byte by byte.
2. **Privileged `unsafe`.** The *decoders* on both sides are already pure:
   `toyos-elf`, `toyos-fat32` and `toyos-gpt` are all
   `#![forbid(unsafe_code)]`. What is left is the effects half, and there
   program loading is the larger: `kernel/src/elf/` + `kernel/src/loader/`
   carry 36 `unsafe` occurrences against `fat32_adapter.rs` 0,
   `bcachefs_adapter.rs` 2 and `gpt.rs` 0 (the `bcachefs` crate carries 11 of
   its own and is the one format crate without `forbid(unsafe_code)`).

On the axis the roadmap actually wrote down — size — the premise is wrong. On
reachability and on privileged `unsafe` it is right, and the spec should say so
in those terms.

---

## 2. Necessity

> *`specs/README.md`: delete the proposed concept. Which required property can
> no longer be guaranteed by the concepts that already exist? Name the workload.
> State the property without the proposed concept's own vocabulary.*

### 2.1 The property

**How much kernel memory and kernel work a program's own file may command is
decided by the kernel, and a mistake in the code that reads that file ends one
program rather than the machine.**

Neither half holds today.

On the first half: every table the loader reads is sized by a number in the
file, and the only thing between that number and `KernelAllocator::alloc`'s
assert is a constant the kernel invented. `mm::MAX_HEAP_ALLOC` (2,093,056) is
applied to `PT_DYNAMIC`, `DT_RELASZ`, `DT_PLTRELSZ`, `DT_STRSZ`, the `.dynsym`
extent, the section header table, `.symtab` and `.strtab` — eight quantities
that a compiler, not the kernel, decides. The constant exists because the
kernel's heap page source cannot serve a larger request, not because 2,093,056
is a meaningful bound on a program.

On the second half: `LoadedLib::write_at` (`elf/reloc.rs:24-50`) asserts,
`AddressSpace::insert_region` asserts, and the heap's page source asserts. All
three are `panic!` in syscall context, which stops the machine. That is the
shape of most of the defect history in §2.3.

### 2.2 The workload

`hosted-rustc = false` in `system.toml` today, and turning it on is the
self-hosting north star. `src/toolchain.rs:1646-1652` refuses the hosted build
unless `librustc_driver*.so` is present, and
`specs/assessments/cranelift-backend-assessment.md` §1.1 records the artifact
that exists. Measured with `toyos-elf` against
`rust/build/x86_64-unknown-toyos/stage2/lib/`:

| artifact | size | span | `align_2m(span)` | RELATIVE | bind | TPOFF64 | DTPMOD64/DTPOFF64 | `DT_RELASZ` | `.dynsym` | `.gnu.hash` | `.symtab`+`.strtab` | private RW window |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| `librustc_driver-….so` | 200,651,072 | 144,760,832 | 146,800,640 | 211,844 | 1 | 236 | 34 / 34 | 5,091,576 | 3,752,520 | 1,250,848 | 55,872,517 | 8,388,608 |
| `librustc_codegen_cranelift-….so` | 21,865,400 | 15,912,960 | — | 17,093 | 735 | 0 | 97 / 97 | 432,528 | 474,288 | 152,176 | 5,013,932 | — |
| `libserde_derive-….so` | 9,055,624 | 6,545,408 | 8,388,608 | 6,505 | 1 | 0 | 165 / 165 | 164,064 | 303,456 | 101,160 | 2,424,517 | 2,097,152 |

`ls rust/build/x86_64-unknown-toyos/stage2/lib/*.so | wc -l` → **18**.

What that workload does to the current arrangement, from the code:

- `load_shared_lib` takes `PageAlloc::new(146_800_640, Category::Elf)` — one
  contiguous 140 MiB physical allocation for `librustc_driver.so`.
- `cache_loaded_lib` pushes it into `SO_CACHE` (`elf/cache.rs:162`), a
  `Lock<Vec<(String, CachedLib)>>` that is only ever `push`ed and read. Nothing
  evicts. `SYS_DLCLOSE` is `syscall.rs:502 => 0` — a literal no-op. So one
  `dlopen` of rustc's driver costs 140 MiB of kernel-owned physical memory for
  the rest of the boot, on a guest the harness gives 4 GB, plus 8 MiB of private
  writable window per process that loads it.
- Each proc-macro dylib rustc loads carries 165 `DTPMOD64` + 165 `DTPOFF64`
  relocations and therefore a TLS module, against a DTV of fixed capacity 64
  (§3).

That is the workload. It is not hypothetical: the artifacts are on disk, built
by this tree.

### 2.3 The two cited defects — both verified, and they are not the whole class

The roadmap cites two. Both are real, and the evidence is in the history.

**`sys_dlopen`'s `init_out` kernel-address write.** Fixed by `dbde7ba`,
2026-08-06, *"kernel: dlopen's init_out was sixteen bytes of kernel memory,
addressed by the caller"*. Its own message:

> `sys_dlopen` handed that number to `AddressSpace::translate`, which walks any
> PML4 index, and `new_user` shallow-copies the kernel half — so a kernel
> address resolves to a present, writable 2 MiB leaf of the direct map and the
> two `core::ptr::write`s land in it. Any process that can call dlopen could
> write two chosen-ish words anywhere in the kernel's address space.

The gate is `tests/toyos-rust-tests/src/bin/abuse_kernel_addr.rs`, and it does
not rest on the syscall's verdict — `SYS_DEBUG` actions 10 and 11 keep sixteen
bytes of kernel memory with a known value and answer whether they still say it.

**`dlopen`'s missing dedup.** `issues/isolation/dlopen-never-dedups.md`
(open, 2026-07-30) is precise about what is closed and what is not: the *panic*
is closed — `find_gap` returning `None` was an `.expect` on three paths in
`spawn` and two in `sys_dlopen`, a kernel panic in syscall context — and the
unbounded VA growth is not. `c7027ba` added the `test-tiny-va` actuator and
`va_exhaustion.rs`, which is the gate for the closed half. So the claim "the
class has produced real defects" is true of this one, but the defect it produced
is the panic, not the dedup; the dedup is still owed.

**Nine dated commits, not two.** `git log --all -- kernel/src/elf
kernel/src/loader kernel/src/elf.rs kernel/src/loader.rs` returns 52 commits.
The ones whose subject is a userland-reachable kernel defect in this code:

| commit | date | subject |
|---|---|---|
| `299a6a0` | 2026-07-28 | elf: make an ELF that lies about its segment sizes unrepresentable |
| `17c72a7` | 2026-07-28 | elf: route every ELF-controlled offset through one bounds-checked accessor |
| `f49c6b3` | 2026-07-29 | Bound a shared library's relocations by the memory they actually write |
| `fcd481f` | 2026-07-30 | Stop SYS_TLS_ALLOC_BLOCK from freeing pages it leaves mapped |
| `679086d` | 2026-08-01 | Close the crafted-ELF panics in the spawn path (+424 lines of corpus) |
| `c7027ba` | 2026-08-01 | vma: test-tiny-va, the actuator for an arena nothing can exhaust |
| `b554798` | 2026-08-01 | elf: bound the allocations the loader *derives*, not just the ones it reads |
| `dbde7ba` | 2026-08-06 | kernel: dlopen's init_out was sixteen bytes of kernel memory |
| `845ee8a` | 2026-08-07 | elf: a section header table that declares no symbols is not a symbol count |

`issues/isolation/derived-allocations-unbounded.md` records the strongest
single item, and records honestly which of its three routes was demonstrated:
Route A was two relocation tables of 87,210 entries each — individually accepted
by `MAX_HEAP_ALLOC`, together producing `GlobalAlloc: dlmalloc asked for 2162688
bytes`. **A real kernel panic from real input.** Routes C and D were fixed on
reading rather than on a reproduction, and the file says so.

The defect class Move 1 removes is therefore not "arbitrary kernel write" — that
one is fixed and gated — but **denial of service of the whole machine by a file
any unprivileged process can write**, plus the memory class in §2.2. Six of the
nine commits above are of exactly that shape.

**Necessity: answered.** The property is real, the current arrangement cannot
have it (the bounds are kernel-invented over compiler-set quantities, and the
failure mode is a machine-wide panic), and the workload that demonstrates the
loss exists on disk today.

---

## 3. Scaling — who sets each quantity

> *`specs/README.md`: for every quantity in every bound the proposal states: who
> sets it — the kernel, the hardware, or the workload?*

### 3.1 The loader's bounds today

| bound | value | site | who sets the quantity it bounds |
|---|---|---|---|
| `MAX_LOAD_SEGMENTS` | 16 | `toyos-elf/src/lib.rs:64` | **workload** (the linker). Measured: every binary in the tree has 3. |
| `MAX_TLS_ALIGN` | 2,097,152 | `toyos-elf/src/lib.rs:74` | **hardware** — the largest page the kernel maps, asserted equal to `PAGE_2M` at `elf/mod.rs:33` |
| `MAX_HEAP_ALLOC` | 2,093,056 | `mm/mod.rs:65` | **kernel** heap; applied to eight **workload**-set table lengths |
| — derived: entries per relocation table | 87,210 | `2_093_056 / 24` | **workload** |
| — derived: `RelocationIndex` u64 entries | 130,816 | `elf/index.rs:81`, `2_093_056 / 16` | **workload** |
| `MAX_GNU_HASH_BYTES` | 65,536 | `loader/mod.rs:56` | **workload** |
| `MAX_SYMBOL_BYTES` | 16,777,216 | `loader/symbols.rs:86` | **workload** |
| `DTV_INITIAL_CAPACITY` | 64 | `loader/tls.rs:16` | **workload** (modules a process loads) |
| `USER_STACK_SIZE` | 8,388,608 | `loader/mod.rs:42` | kernel |
| `USER_VM_BASE` … `alloc_floor()` … `ALLOC_CEILING` | 1 TiB / 8 GiB / `STACK_BASE` | `loader/mod.rs:46`, `vma.rs:33,10` | kernel over hardware |
| `SO_CACHE` size | **none** | `elf/cache.rs:162` | **workload**, and immortal |
| `loaded_libs` per process | **none** | `syscall.rs:2682` | **workload**, and never deduped |

**Seven of twelve are bounds over a quantity the workload sets, and two of them
have no bound at all.** That is exactly the shape three scheduler designs taught
this project to refuse.

### 3.2 Three of them, checked against artifacts this tree has already built

- `MAX_GNU_HASH_BYTES` = 65,536. `libtls_cranelift.so`'s `.gnu.hash` is
  **125,472** bytes (1.9×) and `librustc_driver.so`'s is **1,250,848** (19×).
  The bound only guards the *executable* path (`exe_sym_count`,
  `loader/mod.rs:341-352`) — a library's `.gnu.hash` is a `KernelSlice` over the
  loaded image with no ceiling — and no shipped executable has a `.gnu.hash` at
  all, so nothing hits it today. It is a bound that is already numerically wrong
  and is saved only by being unreachable.
- `MAX_SYMBOL_BYTES` = 16,777,216. Its own comment justifies the number against
  *"`bin/toyos-cc`'s 13,152,031 bytes, and `bin/sshd` … at 3,769,757"*. Measured
  on the 2026-08-15 build: toyos-cc **4,382,380**, sshd **2,953,531**. The
  numbers in the justification are stale by roughly 3×. Filed as
  `issues/design-debt/max-symbol-bytes-justification-is-stale.md`.
- `DTV_INITIAL_CAPACITY` = 64 is the one whose refusal is not recoverable.
  `tls_alloc_block` (`syscall.rs:2724`) returns `ResourceExhausted` above
  it; the only caller is std's `__tls_get_addr_slow`
  (`rust/library/std/src/sys/pal/toyos/tls.rs`), which answers
  `rtabort!("no TLS block for a dlopen'd module")` — because `__tls_get_addr`'s
  ABI is an address and there is nobody to return an error to. 18 `.so`s already
  sit in the hosted sysroot, each proc-macro one carrying 165 `DTPMOD64` pairs.
  Filed as
  `issues/isolation/dtv-capacity-is-a-workload-bound.md`.

### 3.3 What a Ring 3 loader would need

Nearly none of the above. In Ring 3 the quantity and the account are the same
process's: a relocation table's `Vec` is charged to the process that asked for
it, bounded by that process's own address space and by the PMM, both of which
are bounds the kernel already enforces for every other allocation. `MAX_HEAP_ALLOC`
and its two derived ceilings, `MAX_GNU_HASH_BYTES` and `DTV_INITIAL_CAPACITY`
have no reason to exist there. `MAX_LOAD_SEGMENTS` exists so `Layout` needs no
allocator; a Ring 3 loader has one, so it could go too, though keeping it costs
nothing.

**This is the strongest technical argument for Move 1, and it is not the one the
roadmap makes.** Every ceiling in the loader is an invented number standing in
for a resource limit the kernel already has elsewhere, and every one of them
either refuses a legitimate large program or degrades it silently. Moving the
code moves the accounting to the account that should carry it.

What the Ring 3 loader would need *from* the kernel, checked against what
`SYS_MMAP` gives today (`syscall.rs:2040`, `toyos-abi/src/syscall.rs:553`):
`ANONYMOUS | PRIVATE | FIXED`, `READ | WRITE | NONE`, 2 MiB granularity, no file
backing and no `mprotect`. So a Ring 3 loader must `read()` a library into
anonymous memory rather than map it, and cannot mark its text read-only.

That second point looks like an isolation regression and is not: `vma_map`
(`process.rs:36-42`) passes `writable = true` unconditionally for every library
image today, and the demand-fault path (`process.rs:1542`) maps a 2 MiB page
writable if *any* region overlapping it is writable. Library text is already
writable in this kernel. Move 1 is neutral on W^X, and it removes the
cross-process half of the problem recorded in
`issues/isolation/kernelslice-over-user-memory.md`: today one cached image
is mapped, writable, into every process that loaded that path.

The first point is a real cost: a Ring 3 loader gets no page-cache sharing, so
N processes loading the same library hold N private copies. With the shipped
system loading zero libraries the cost today is zero; with hosted rustc it is
one 140 MiB copy per rustc process instead of one shared 140 MiB copy plus
8 MiB per process. **That trade should be stated in the spec and signed off, not
discovered.**

---

## 4. Authority and blast radius

> *`specs/README.md`: does the mechanism assert what its own site observes, or
> does it predict what another site will do later?*

### 4.1 What the kernel asserts about a loaded image today

1. `Layout::parse`'s invariants (`toyos-elf/src/layout.rs:112-135`): `ET_DYN`,
   `EM_X86_64`, 1–16 `PT_LOAD`s, `filesz <= memsz`, no `vaddr + memsz` or
   `file_offset + filesz` overflow, `entry` and the file-backed extents of
   `PT_TLS`/`PT_DYNAMIC`/`PT_GNU_EH_FRAME` inside `[vaddr_min, vaddr_max)`,
   `tls.align` a power of two ≤ 2 MiB.
2. `image_fits_user_half` (`loader/mod.rs:132`): the rebased image lies wholly
   in the user half.
3. `overlapping_load_pages(4096)` (`loader/mod.rs:151`): no two `PT_LOAD`s
   contend for a page.
4. `rela::validate` (`toyos-elf/src/rela.rs:229`): **every** relocation the
   loader will ever write lands inside the module's writable window, and every
   symbol-consuming entry's `r_sym` is inside `.dynsym` — checked before the
   first write, because a module refused halfway has already been modified.
5. `ModuleImage::slice` (`elf/mod.rs:199`): every `DT_*` vaddr resolves inside
   the loaded image, as a refusal rather than an assert.
6. Every declared table length is ≤ `MAX_HEAP_ALLOC`.

Assertions 1–3 are about the image the kernel is about to map, and it observes
them at the site. **Assertions 4–6 are of the other kind**: they are the kernel
guaranteeing, on behalf of a program, that the program's own file will not
corrupt the program. That is a prediction about another site, and under
`specs/README.md`'s third question it should be measurement, accounting or
policy — not a kernel invariant.

### 4.2 What it would stop asserting

1–3 survive Move 1; the kernel still maps the `PT_LOAD`s. 4–6 leave. So the
kernel would stop guaranteeing that a program's relocations stay inside the
program, and that is the *correct* answer to the third question: it never had
authority over that, and holding it is what turned a program's malformed file
into a machine-wide panic nine times.

### 4.3 What a crafted binary reaches, in each arrangement

**Today**, from `fs::write` + `spawn`/`dlopen` in an ordinary guest program:

- kernel heap allocations sized from eight file-declared lengths, where the
  ceiling is all that stands between the file and an assert in
  `KernelAllocator::alloc`;
- 29 privileged `unsafe` occurrences in the code that leaves under Move 1;
- three `panic!`-in-syscall-context asserts (`LoadedLib::write_at`,
  `insert_region`, the heap's page source), each of which stops the machine;
- an immortal, unbounded, machine-wide `SO_CACHE` keyed by path string with no
  revalidation of the file behind it;
- a shared physical image mapped writable into every process that loaded the
  same path;
- up to 16,777,216 bytes of kernel pages per process from `.symtab`/`.strtab`.

**After Move 1**: `Layout::parse` (pure, `forbid(unsafe_code)`, host-tested
against `toyos-elf/tests/crafted.rs`, 358 lines), `insert_elf_regions`, the
demand-fault file read, and `read_backtrace_table`. A crafted `.so` reaches the
Ring 3 loader's own heap and the target process's own address space, and
`sys_mmap` already refuses the placements that matter (kernel half, misaligned,
overlapping another region). Blast radius: one process.

### 4.4 Consumers that must change

| consumer | what changes | verified how |
|---|---|---|
| `toyos-abi/src/syscall.rs` | five numbers retired (55, 56, 57, 88, 91), `SYS_THREAD_SPAWN` (40) gains `fs_base`; the `dl_open`/`dl_sym`/`dl_close`/`tls_alloc_block`/`query_modules` wrappers go | `grep -rn 'SYS_DL'`, §1.3 |
| `userland/libc/src/misc.rs:430-456`, `include/dlfcn.h` | the three C shims re-point at the Ring 3 loader | grep |
| `rust/` — the **`libloading` fork** | `forks.toml` [libloading]: base `35b6a30`, delta **+341/-4**, tier `toolchain`, `why = "dlopen/dlsym over ToyOS."` The whole delta is these syscalls. | `forks.toml:136-144` |
| `rust/library/std/src/sys/pal/toyos/tls.rs` | `__tls_get_addr` + `__tls_get_addr_slow` — naked asm over the DTV plus the `tls_alloc_block` slow path | read |
| `rust/library/backtrace/src/symbolize/gimli/libs_toyos.rs` | `native_libraries()` is a `query_modules` call | read |
| **std itself** | **nothing.** `grep -rn "dl_open\|dl_sym\|dl_close\|SYS_DLOPEN" rust/library` → 0 hits. std never dlopens; only the `libloading` fork does. | grep |
| `toyos-elf` | **nothing.** The crate is pure and moves from the kernel's link to a userland one unchanged. Its 358-line crafted corpus stays a host test. | read |
| test corpus (9 guest binaries) | `std_tls_dlopen`, `std_tls_multi_crate`, `std_tls_cranelift`, `std_unwind_so`, `abuse_tls_alloc` (whole file is `SYS_TLS_ALLOC_BLOCK`), `abuse_kernel_addr` (its dlopen half; the futex half survives), `abuse_elf_loader`'s `dlopen_survives` cases, `abuse_elf_segments`'s `dlopen_err` cases, `va_exhaustion`'s dlopen assertion | grep |
| `system.toml` | **nothing.** No shipped program is dynamically linked (§0) and no `.so` reaches a boot image. | §0 |

**The absence of `PT_INTERP` is what keeps this tractable.** `grep -rn
"PT_INTERP" --include='*.rs' .` → 0 hits, and `toyos-elf` accepts `ET_DYN`
only. Because nothing shipped is dynamically linked, Move 1 needs **no
interpreter**: a static-PIE self-relocates and jumps to its own entry, and the
Ring 3 dynamic loader can be an ordinary library linked into the one program
that calls `dlopen`. The kernel gains no "open the interpreter by name" policy,
which would have been a connect-by-name in a capability system.

---

## 5. Cost and sequencing

### 5.1 Chunks

Sizes below are **estimates** except where a measured line count is given.

1. **Self-relocating startup.** The exe's `RELATIVE` relocations move from the
   kernel's `RelocationIndex` to a startup stub that runs before any code
   touching a GOT-relative global. It lands in `rust/` (std's toyos `rt`) or
   `userland/libc`, and it must work for `/bin/init`, which the kernel spawns
   first — a mistake is an unbootable image. Estimated small in lines, high in
   care. **Its cost at runtime is measured and it is nil**: every shipped
   binary's writable window fits inside one 2 MiB page (`init` 73,728 bytes,
   `shell` 69,632, `compositor` 221,184, `sshd` 290,816, `toyos-cc` 376,832),
   so eager self-relocation faults in exactly the page the program was going to
   touch anyway.
2. **Delete the exe relocation path from the kernel**: `elf/index.rs` (164) and
   the fault handler's application (`process.rs:1565` and `:1624-1635`, 13
   lines) — 177 measured lines — plus the rela half of `read_exe_tables` and the
   `ElfInfo.reloc_index` field it fills.
3. **A Ring 3 dynamic loader** over `toyos-elf` + `SYS_MMAP` + open/read.
   Estimated 800–1,200 new userland lines; it needs no new kernel mechanism
   (§3.3).
4. **TLS moves to Ring 3**, taking `loader/tls.rs` (207), `sys_tls_alloc_block`
   (71) and `SYS_THREAD_SPAWN`'s `fs_base` with it. This is the largest ABI
   step and the one the roadmap does not mention. A cheaper variant exists and
   should be priced: the kernel keeps the *exe's* single static TLS module —
   which, measured, is all any shipped process ever has (module count 1, memsz
   144–560) — and userland owns everything a `dlopen` adds. That variant retires
   `SYS_TLS_ALLOC_BLOCK` and leaves `SYS_THREAD_SPAWN` alone.
5. **Retire the five syscalls**, re-point `libloading`, std's `tls.rs` and
   `libs_toyos.rs`, and re-aim the nine guest tests.
6. **Delete `kernel/src/elf/` and most of `kernel/src/loader/`**: 2,177 measured
   kernel lines, 29 `unsafe` occurrences, plus 600 lines of `toyos-elf` off the
   kernel's link.

### 5.2 Independence from the completion architecture — measured, and true

`specs/plans/kernel-slimming-roadmap.md` claims Move 1 is "independent of the
completion architecture". Checked against PR #91 (`wt/toyos-p2impl`, chunks C1
and C2 of `specs/completion-architecture-spec.md`):

```
$ git diff --stat origin/main...FETCH_HEAD -- kernel/src/loader kernel/src/elf kernel/src/symbols.rs
 kernel/src/loader/mod.rs | 2 +-
 1 file changed, 1 insertion(+), 1 deletion(-)

$ git diff origin/main...FETCH_HEAD -- kernel/src/arch/syscall.rs | grep -c '^@@'   →  12
$ … | grep -n 'dlopen\|dlsym\|tls_alloc_block\|query_modules\|loaded_libs\|elf\.'  →  (none)
```

Pipeline 2 touches the loader by exactly **one line**, touches
`kernel/src/elf/` and `kernel/src/symbols.rs` not at all, and none of its twelve
`arch/syscall.rs` hunks is in a `dl`/`tls`/`query` arm. The independence claim
holds at file level, not merely at design level.

There is a *positive* coupling worth recording, in the other direction:
`spawn` today does every library load and the whole `.symtab` read synchronously
inside the caller's syscall, in a kernel where — `kernel/CLAUDE.md` — *"No disk
wait in this kernel can park"*. A Ring 3 loader's file reads happen in the
child's own scheduled context. That is a latency improvement Move 1 delivers for
free and completions would deliver anyway; neither blocks the other.

### 5.3 The ABI split

Five syscall numbers retire and one signature changes. `CLAUDE.md`: *"An ABI
change lands on its own PR first"*. So Move 1 is at least two pull requests
before any deletion lands, and probably three: the ABI retirement, the Ring 3
loader plus consumers, and the kernel deletion. `Abi-Inseparable:` would be
needed only if the TLS step genuinely cannot be split from the loader step,
which the §5.1 item 4 variant suggests it can.

---

## 6. Defects filed

Three, found while measuring, filed rather than fixed:

| finding | issue |
|---|---|
| `SO_CACHE` never evicts, has no bound, and is keyed by path with no revalidation of the file behind it | `issues/isolation/so-cache-never-evicts.md` |
| `DTV_INITIAL_CAPACITY` is a fixed bound over a workload-set quantity whose refusal is `rtabort!` in std | `issues/isolation/dtv-capacity-is-a-workload-bound.md` |
| `MAX_SYMBOL_BYTES`'s justification cites 13,152,031 and 3,769,757 bytes; measured 4,382,380 and 2,953,531 | `issues/design-debt/max-symbol-bytes-justification-is-stale.md` |

Two claims in `specs/assessments/2026-08-15-mechanism-consolidation-audit.md`
§1.7 are, at `af1b52d`, no longer true and are recorded here rather than edited
there (a dated record's body is never rewritten): `kernel/src/symbols.rs` no
longer holds a second ELF decoder on crates.io `elf` 0.8 — `e0186e2` moved the
scan onto `toyos-elf`, and `symbols.rs:211-221`'s `tables()` is now the whole of
its raw-pointer surface — and `bootloader/src/main.rs` decodes through
`toyos_elf` too (`bootloader/Cargo.toml:11`). `grep -n '^elf\b\|^elf =\|"elf"'
kernel/Cargo.toml` → no match; the crate's only ELF dependency is `toyos-elf`
(`kernel/Cargo.toml:112`). **`specs/plans/kernel-slimming-roadmap.md`'s
Move 3 is therefore partly stale**: its claim that `symbols.rs` *"re-implements
the symtab walk over raw pointers, bypassing the hardened crate"* no longer
holds, and its "463 lines together" is 501 (`symbols.rs` 336 +
`loader/symbols.rs` 165). What survives of Move 3 is the `rustc-demangle`
question and the absence of host tests.

---

## 7. Recommendation

**Do it — after the completion architecture lands, and before `hosted-rustc` is
turned on.** The evidence says the roadmap's stated premise is wrong and its
underlying instinct is right: program loading is the *third*-largest
untrusted-input parser in Ring 0 (5,152 lines against the filesystem's 8,419 and
USB's 7,787), not the largest, so "largest parser" is not the argument. The
argument that the measurements do support is the one about bounds — seven of the
loader's twelve ceilings are kernel-invented numbers over quantities a compiler
sets, two of them are already exceeded by artifacts sitting in this tree, two
have no bound at all, and the failure mode when one bites is a `panic!` in
syscall context reached from a file any unprivileged process can write, which is
the shape of six of the nine hardening commits this code has already needed.
Moving the work to Ring 3 does not merely shrink the kernel by 2,177 lines and
29 `unsafe` occurrences; it deletes the need for those ceilings entirely, because in
Ring 3 the account that pays is the account that chose the quantity. There is no
urgency today — the shipped system executes none of this code, and Move 1's cost
is five retired syscall numbers, a changed `SYS_THREAD_SPAWN`, a self-relocation
stub whose failure mode is an unbootable `/bin/init`, and three pull requests —
but there is a deadline: the day `hosted-rustc` is turned on, a 200 MB
`librustc_driver.so` is dlopened into a kernel whose shared-object cache never
evicts, and every one of those bounds becomes load-bearing at once. Doing it
before that is a deletion; doing it after is a rescue.
