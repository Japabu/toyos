# Boot image: symbols, root filesystem, toolchain, profile

> Rewritten 2026-08-07 against `main` at `43ce73e`. The 2026-07-29 version of
> this document designed a two-tier split whose central premise — that ToyOS has
> no USB mass-storage driver and that building one costs 1,200–2,000 lines of
> kernel — **is false**. The driver exists, boots the owner's T14, and is the
> device every `cargo test` in this tree reads its boot volume through. Its whole
> §3 and the ordering in its §7 keyed off that cost. What survives is the size
> accounting, re-measured here off a current image, and the memory argument.
>
> Every number below comes from a command run against this tree. Where a figure
> has not been measured it says so and does not appear in a table.

## 0. What this decides

Four changes, in dependency order, each landing on its own and each leaving
`main` green.

1. **Symbolication reads a file** rather than pointers into initrd RAM. This is
   the only thing that makes the initrd architecturally special, and it is a
   prerequisite for 2 in the sense that matters: fix it first and removing the
   initrd costs nothing in diagnostics.
2. **Delete the initrd.** The root filesystem becomes a partition on the boot
   medium, read through the same `BlockDevice` the kernel already reads `/boot`
   and `/log` through.
3. **`hosted-rustc = false`.** Decided by the owner. 82.6% of the initrd's
   content is a toolchain that no boot invokes.
4. **One named build profile**, stating all four of `--release`'s knobs, applied
   to every binary the image carries.

And one hypothesis, tested and **rejected** in §2.3: that the heavy DWARF could
live on the volume and be read only when a backtrace is taken. There is no
DWARF. `toyos-ld` drops every debug section, so `.symtab`/`.strtab` — the thing a
named backtrace needs — *is* the entire debug weight, and it is 32.2% of every
shipped binary.

## 1. What is actually in the image

### 1.1 Measured

`target/bootable.img`, built 2026-08-07 15:21 from `43ce73e`, read by parsing its
GPT, both FAT32 volumes and the initrd's bcachefs (`examples/imgstat.rs`, a
throwaway):

| | Bytes | MiB |
|---|---|---|
| whole image | 729,808,896 | 696.0 |
| ESP partition (`TOYOS-BOOT`) | 691,777,536 | 659.7 |
| — `/toyos/initrd.img` | **672,083,968** | **641.0** |
| — `/toyos/kernel.elf` | 3,740,104 | 3.57 |
| — `/EFI/BOOT/BOOTx64.EFI` | 946,176 | 0.90 |
| — `/toyos/log.guid` | 16 | — |
| log partition (`TOYOS-LOG`) | 35,651,584 | 34.0 |

The initrd is 92.1% of the whole image and 97.2% of the ESP the bootloader reads.

### 1.2 Inside the initrd

131 entries, 610,704,287 bytes of content in a 672,083,968-byte volume:

| Group | Entries | Bytes | % of content |
|---|---|---|---|
| hosted rustc — `lib/` | 79 | 504,214,834 | 82.56 |
| hosted rustc — `bin/rustc` shim | 1 | 18,248 | 0.00 |
| userland `bin/` (17 binaries + 19 symlinks) | 36 | 89,985,665 | 14.73 |
| assets `share/` | 15 | 16,485,540 | 2.70 |
| **content** | **131** | **610,704,287** | **100** |
| bcachefs slack + metadata | — | 61,379,681 | 9.13% of the volume |

Largest entries:

| Bytes | Entry |
|---|---|
| 200,651,072 | `lib/librustc_driver-ea85c2166f476ad5.so` |
| 67,590,886 | `lib/rustlib/x86_64-unknown-toyos/lib/libcore-*.rmeta` |
| 32,977,320 | `bin/toyos-cc` |
| 21,865,400 | `librustc_codegen_cranelift-1.99.0-dev.so` |
| 12,245,032 | `bin/sshd` |
| 6,827,984 | `bin/toyos-ld` |
| 6,220,808 | `share/wallpaper.rgb` |
| 5,994,284 | `share/timgm6mb.sf2` (no longer shipped — §5) |
| 5,225,776 | `bin/compositor` |
| 5,161,144 | `bin/files` |
| 4,196,020 | `share/doom1.wad` |
| 4,175,368 | `bin/doom` |

### 1.3 Two entries nobody put there

`system.toml`'s `assets = ["assets"]` sweeps the directory whole
(`src/assets.rs:198-213` recurses and takes every file that is not `.ttf` or
`.jpg`). Two files in the working tree that git does not track therefore ship:

- `share/.ds_store` — 6,148 bytes, written by Finder.
- `share/target/.deps-stamp` — 10,220 bytes, from a stray `assets/target/`.

Neither is in a fresh clone, so **a fresh clone builds a different image**, and
opening `assets/` in Finder moves the image hash with no code change. Fixed in
§5.

### 1.4 What the build actually optimises, per binary

The 2026-07-29 document said "everything is an unoptimized, unstripped debug
build". That has not been true for some time, and what replaced it is
inconsistent. Read off the manifests:

| Binary | Profile that applies | opt-level | debug-assertions | overflow-checks |
|---|---|---|---|---|
| `kernel` | `kernel/Cargo.toml [profile.dev]` | **2** | on | on |
| `bootloader.efi` | cargo's `dev` default | **0** | on | on |
| `compositor`, `doom` | `userland/Cargo.toml [profile.dev.package.*]` | **2** | on | on |
| every other userland program | cargo's `dev` default | **0** | on | on |
| every userland *dependency* | `[profile.dev.package."*"]` | 2 | on | on |
| `toyos-cc`, `toyos-ld` (guest) | their own crates, no profile | **0** | on | on |

So the compositor's own code is optimised and the terminal's is not; the kernel
is and the bootloader is not; `toyos-cc` ships 32.98 MiB at opt-level 0. There is
no stated policy anywhere — `kernel/Cargo.toml:371` carries the only sentence on
the subject, and it is about the two knobs that are *not* varying:

```
# debug-assertions and overflow-checks stay on: fail-fast beats speed here.
```

§6 makes that one profile and applies it everywhere.

## 2. Symbols

### 2.1 What the initrd is load-bearing for

`kernel/src/loader/mod.rs:578-589` builds a process's `SymbolTable` through
`process::find_symtab_in_memory`, which asks the backing for
`FileBacking::memory_ptr` (`process.rs:1672-1677`). That method is `None` by
default and implemented **only** by `InitrdBacking`
(`file_backing.rs:37-39, 204-214`). Everything else in the loader is written
against `FileBacking::read_page` and works from any device — `/home` proves it.

So a program run from a disk today loses its symbol names, silently, and the
initrd is the only reason any program has them. That is the whole of the
initrd's architectural specialness.

### 2.2 Nothing tests it

`check_panic_recovery` (`tests/toyos.rs:898-925`) asserts that a SEGFAULT report
contains `deliberate_null_deref` — a real demangled-name gate, and the only one.
It is satisfied by `/bin/test_rs_segfault_child`, which `build_toyos_bins` stages
into the **initrd** as an extra file (`tests/toyos.rs:9760-9762`).

**No test in this tree asserts that a program running from a disk gets a named
backtrace.** Deleting the initrd would take the only coverage with it and nothing
would go red. §7 makes that gate exist before stage 2 lands.

### 2.3 The DWARF hypothesis, tested and rejected

The proposal was: `.symtab` is small, full DWARF is the bulk, so put DWARF on the
volume and read it lazily. Measured with `readelf -S` on the linked binaries:

| Binary | Total | `.symtab`+`.strtab` | share |
|---|---|---|---|
| compositor | 5,225,776 | 1,309,346 | 25.1% |
| toybox | 2,440,160 | 702,808 | 28.8% |
| soundd | 2,262,456 | 611,116 | 27.0% |
| netd | 2,279,896 | 620,365 | 27.2% |
| sshd | 12,245,032 | 3,769,757 | 30.8% |
| toyos-ld | 6,827,984 | 2,272,581 | 33.3% |
| toyos-cc | 32,977,320 | 13,152,031 | 39.9% |
| kernel | 3,740,104 | 548,666 | 14.7% |
| **all 18 shipped ELF binaries** | **92,138,384** | **29,687,242** | **32.2%** |

And the sections a linked ToyOS binary has, in full:

```
.text  .strtab  .symtab  .rela.dyn  .data  .eh_frame_hdr  .dynamic  .shstrtab
```

**There is no `.debug_*` section in any binary this project produces.**
`toyos-ld` drops them at collection: `collect.rs:410-416` matches
`SectionKind::Debug | DebugString | Linker | Note | Metadata` and `continue`s.

Three consequences:

- The hypothesis is wrong in its premise. There is nothing heavy to move,
  because `.symtab`/`.strtab` is the whole of it — and that is precisely what a
  named backtrace needs, so it cannot be the part left behind.
- **`debug = true` in `kernel/Cargo.toml` buys nothing.** rustc emits DWARF into
  the object files and the linker throws it away. It costs compile time and
  produces no artifact. §6 keeps it anyway for one reason and says which.
- Backtraces with **line numbers** are not currently possible at all, on any
  path, and would need `toyos-ld` to keep `.debug_line` before anything else.
  Out of scope here; recorded so the next reader does not go looking.

Compaction does not help either. Keeping only `STT_FUNC` symbols with a nonzero
value, and only their names:

| Binary | symbols | FUNC, nonzero | name bytes | compacted | vs raw |
|---|---|---|---|---|---|
| compositor | 14,803 | 13,983 | 969,118 | 1,192,846 | −8.9% |
| toybox | 7,407 | 7,299 | 534,494 | 651,278 | −7.3% |
| sshd | 28,843 | 27,279 | 3,083,132 | 3,519,596 | −6.6% |
| toyos-cc | 74,268 | 74,174 | 11,450,056 | 12,636,840 | −3.9% |

Nearly every symbol is a function and the mangled names are the bulk. A
compacted index is under ten percent smaller and costs a second format nothing
else reads. Not worth it.

### 2.4 Therefore: stage 1's shape

`SymbolTable` (`kernel/src/symbols.rs`) resolves by linear scan over raw
pointers, with no allocation and no lock, because it is called from the panic
handler and from the fault path. **That property is not negotiable and stage 1
keeps it.** What changes is only where the bytes come from.

- The kernel's own table keeps pointing into the kernel ELF the bootloader left
  in the direct map (`main.rs:372`). It is mapped for the machine's life.
- A process's table is **read off its file at spawn into contiguous 2 MiB pages**
  (`process::PageAlloc`, `Category::Elf`), and `SymbolTable::from_raw` points at
  the direct map of those pages exactly as it points at the initrd today. The
  process owns the allocation; `PageAlloc`'s drop returns the pages when it
  exits.

A `Vec` will not do: `mm::MAX_HEAP_ALLOC` is 2,093,056 bytes and sshd's tables
are 3,769,757, toyos-cc's 13,152,031. `loader::symbols::read_symtab` — which
already reads both tables whole from any backing, for dynamic linking — refuses
above that bound today, so the two callers converge on the page-scale reader.

Cost, rounded to the 2 MiB granularity `PageAlloc` allocates in:

| Process | tables | pages held |
|---|---|---|
| toybox | 702,808 | 2 MiB |
| soundd | 611,116 | 2 MiB |
| netd | 620,365 | 2 MiB |
| compositor | 1,309,346 | 2 MiB |
| sshd | 3,769,757 | 4 MiB |
| toyos-cc | 13,152,031 | 14 MiB |

The boot set (compositor, soundd, netd) is 6 MiB, against the 641 MiB stage 2
gives back. The residual is that every short-lived `ls` costs 2 MiB for as long
as it runs, and that two `terminal` processes hold two copies of one binary's
table. Both are bounded by `MAX_SYMBOL_BYTES` (§7) rather than left open, and
sharing one table per executable is the obvious follow-up — it needs a lifetime
story this stage does not, so it is not in it.

Reading lazily at backtrace time was considered and rejected: it puts block I/O
on the panic path, which is reached from contexts (`halt_all_cpus`, double
fault) where nothing may block.

## 3. The initrd, and why it can go

### 3.1 The kernel already has block devices two phases earlier

`kernel/src/main.rs`, phases in order:

| Phase | Does | Needs the initrd? |
|---|---|---|
| 1 Memory | `mm::init`, **reserves the initrd region** | No |
| 2 CPU | MADT, LAPIC, IDT, syscalls, kernel symbols, HPET, RTC, timer | No |
| 3 Storage | ECAM, PCI, IOMMU, file cache, GPT identity, **NVMe**, page cache, `/home` | No |
| 4 Peripherals | **xHCI**, **`fat32_adapter::probe_boot_disks`**, i8042, ACPI power | No |
| 5 Subsystems | SMP, VFS, process, scheduler, pipes, io_uring, shm; **mount initrd as `/`**; `/home`; `/tmp`; **`/boot`**; **`/log`** | Yes, here |
| 6 Devices | virtio console, net, sound, GPU | No |
| 7 Userland | spawn `init_programs` | Yes |

`/boot` and `/log` are already mounted in phase 5 off the boot medium's GPT, by
GUID, through `BlockDevice`. A third partition mounted as `/` is the same
mechanism a third time.

### 3.2 The prerequisite the old document priced is already paid

Its §3 listed eight missing pieces and estimated 1,200–2,000 lines of kernel.
Every one of them is built:

| Old §3.2 item | Where it lives now |
|---|---|
| bulk endpoints, per-endpoint rings | `kernel/src/drivers/xhci/` — 3,317 lines across 5 files, plus `toyos-xhci/` |
| caller-supplied data buffers | same |
| class 8 / subclass 6 descriptor parsing | `xhci/device.rs` |
| Bulk-Only Transport | `xhci/wait/msc.rs`, 1,125 lines |
| SCSI | same |
| non-spinning transfers | `toyos_xhci::{job,recovery}`, one outstanding op per controller |
| a second block device | `block::DeviceId`; `usb_storage.rs` takes 16.., NVMe takes 1 |
| a multi-device page cache | **not built — see below** |

`kernel/src/drivers/usb_storage.rs` is a 110-line `BlockDevice` over it. The T14
boots off it; so does every `cargo test`, because the emulated boot medium is
`usb-storage,bus=xhci.0` (`src/qemu.rs:120`, `tests/common/qemu.rs:2092`).

That last fact also retires the old §5.3 / R2 proposal to swap the dev loop to
`virtio-blk`. Doing so now would remove the *only* device that exercises the USB
mass-storage path on every run — which is exactly the failure the old §6.5
warned about, arriving from the other direction.

**The eighth item is still open, and it is stage 2's real cost.**
`page_cache.rs:11-12` holds one `BLOCK_DEV: Lock<Option<Box<dyn BlockDevice>>>`,
`PageCache::_device_id` is still written and never read, and `page_cache::init`
takes ownership of the NVMe device. So `usb_storage.rs`'s own comment — "the
page cache keys itself on this" — describes something the page cache does not
do, and `fat32_adapter.rs:911-915` says the consequence out loud: a machine that
boots off an *internal* disk gets neither `/boot` nor `/log`, "because the NVMe
device is owned by the page cache from the moment storage comes up and there is
no second handle to it".

`/boot` and `/log` work today only because they are USB, and `usb_storage::open`
mints a fresh handle per call. A bcachefs root on the boot medium needs a
`BlockIO` over an arbitrary `BlockDevice` at a partition offset with a cache of
its own — `PageCacheBlockIO` is the NVMe device by construction, and
`FatDevice`'s eight-block resident set is the shape but not the size. That is
the work §3.4 costs, and it is a page-cache change rather than a driver.

### 3.3 What the initrd costs

`main.rs:348` hands `mm::init` the initrd as a reserved region and
`mm/pmm.rs:215-227` simply never marks reserved pages free. Nothing returns
them. **641.0 MiB of a 2 GiB guest — 31.3% of RAM — is held for the machine's
life**, of which 82.6% is a toolchain no boot invokes.

The boot working set against it, from the current `system.toml` init list:

| | Bytes |
|---|---|
| `bin/compositor` | 5,225,776 |
| `bin/soundd` | 2,262,456 |
| `bin/netd` | 2,279,896 |
| `share/fonts/JetBrainsMono-Regular-8x16.font` | 54,920 |
| `share/wallpaper.rgb` | 6,220,808 |
| **total** | **16,043,856 = 15.30 MiB, 2.20% of the image** |

An upper bound: `insert_elf_regions` (`loader/mod.rs:445`) demand-pages every
segment through `FileBacking::read_page`, so only touched pages are ever read.

### 3.4 Shape

**A third GPT partition on the boot medium, holding a bcachefs volume, mounted
as `/`.** Everything it needs exists:

- `bcachefs_adapter` already mounts a `Mounted<PageCacheBlockIO, _>`; the
  read-only initrd path is the only thing that uses `SliceBlockIO`, and it goes.
- Identity is a GUID in `KernelArgs`, minted by `src/image.rs` and written to the
  ESP beside `log.guid`, exactly as the log partition's is
  (`image.rs:243-274`, `gpt.rs:82-101`). `gpt::probe` finds it by GUID on the
  device that carries the boot partition. **No probing by type or by format**,
  which is the invariant `gpt.rs`'s module comment exists to hold.
- `dd`-ing one file to a stick still produces a correct three-partition medium.

Phase order does not move: the mount joins `/home`, `/tmp`, `/boot` and `/log`
in phase 5, and `set_root` takes it instead of `mount_initrd`.

`KernelArgs.initrd_addr`/`initrd_size`, the bootloader's `load_file_bytes` of
`\toyos\initrd.img`, `InitrdBacking`, `ReadOnlyBcacheFsAdapter`,
`FileBacking::memory_ptr` and `SliceBlockIO` all go with it. That is the
deletion this stage is worth doing for, over and above the RAM.

**Failure policy.** A machine whose root partition is absent or unreadable has no
userland at all, so unlike the old tier-2 design there is nothing graceful to
degrade to: it is a boot failure and says which GUID it could not find. That is
not a change in kind — `main.rs:475` already asserts `!initrd.is_empty()`.

## 4. `hosted-rustc = false`

`system.toml`. Removes the 79 `lib/` entries and the `bin/rustc` shim —
504,226,197 bytes measured on the stage-4 image, 86.17% of its content. Decided
by the owner; `specs/posix-bootstrap-cost.md` and the self-hosting north star
are what bring it back, in a `--dev-boot` image the owner has deferred until
that work.

Only the *image* changes. `toolchain::ensure` still builds the hosted rustc — it
is keyed on `hosted-rustc.stamp` and not on `system.toml` — so
`hosted-rustc = true` still works and costs nothing to turn back on. Stopping
the build would mean rewriting `write_config`'s bootstrap options, which
rebuilds the shared sysroot every worktree reads; not worth it for an artifact
that is built once.

`collect_hosted_rustc` is called only under the flag, so nothing in the build
system assumes the sysroot is there when it is false — `cargo test` has always
built this way, since `tests/testcases/system.toml` has no `hosted-rustc` key.

`share/hello.rs` (55 bytes) exists for the hosted rustc and has no other
consumer. Left in place: it is git-tracked, it is 55 bytes, and it is what
somebody will want the moment `hosted-rustc = true` goes back on.

**This stage has no gate, and that is a finding rather than an omission.**
Nothing in the harness boots `system.toml` at all — every test config is its
own file — so no suite test could go red for this change in either direction.
Recorded in `known-issues.md` §6.

### 4.1 What it measured

| | stage 4 | stage 3 | |
|---|---|---|---|
| whole image | 701,497,344 | **137,363,456** | −80.4% |
| ESP partition | 662,794,240 | 99,385,344 | −85.0% |
| `toyos/initrd.img` | 643,944,448 | 89,337,856 | −86.1% |
| initrd content | 585,120,562 | 80,942,013 | −86.2% |
| initrd entries | 129 | 49 | |

Against where this task started — 729,808,896 bytes on `43ce73e` — the whole arc
is **−81.2%, a 5.31× reduction**, and none of it cost a feature: the toolchain
is one boolean away and every binary that was in the image is still in it.

What is left in the initrd is 64,472,841 bytes of userland binaries (79.65%) and
16,469,172 of assets (20.35%). The three largest entries are now `bin/toyos-cc`
at 14,958,480, `bin/sshd` at 10,312,256 and `share/wallpaper.rgb` at 6,220,808 —
so a next tier, if anyone wants one, is the C toolchain at 18.4 MiB and doom's
two assets at 9.7 MiB.

**The memory figure moves with it**, and that was §3.3's durable argument: what
`mm::init` reserves and never frees goes from 641.0 MiB of a 2 GiB guest to
**85.2 MiB**. Stage 2 takes the rest.

## 5. The asset sweep

`assets = ["assets"]` is a directory, and `src/assets.rs`'s `add_dir` takes
everything in it. The image is therefore a function of the working tree rather
than of the commit.

**Built**, and the first attempt was wrong in the other direction. Filtering to
what `git ls-files` returns is right about `.DS_Store` and about the stray
`target/`, and wrong about the one file in that directory git deliberately does
not carry: `assets/timgm6mb.sf2`, 5,994,284 bytes of SoundFont doom synthesises
its music from. The image built after that commit did not have it, and the whole
suite was green either way — **nothing in this tree gates doom's music**, which
is a finding of its own.

So `untracked-assets` in each config names the paths that ship without git
having a copy. Both halves of what that meant changed on 2026-08-08. TimGM6mb is
GPL-2.0 under an MIT OR Apache-2.0 tree, so it is gone from the repository
altogether and §1.2's 5,994,284-byte entry with it; music is opt-in, from a
`.sf2` the owner of a build drops in himself. And a declared entry the build
cannot find is now named and skipped rather than fatal — the naming is what the
hard error was for, the stopping was what made a fresh clone, a runner and every
new worktree red on a file none of them was ever going to have.

## 6. One profile

`--release` bundles four knobs and this project wants three of them. Shipping
stock `--release` silently turns overflow checks off, and `known-issues.md`'s
"two crafted-ELF kernel panics the first hardening wave did not reach" is what
that costs: both were found *because* the kernel builds with `overflow-checks`
on, and one of them — `e_phoff + ph_entry_size * e_phnum` in `usize` — was a
panic with the checks on and an inverted slice range with them off, so there was
no configuration in which it was an error return.

| knob | cargo `dev` | cargo `release` | ToyOS |
|---|---|---|---|
| opt-level | 0 | 3 | **2** |
| debug-assertions | on | off | **on** |
| overflow-checks | on | off | **on** |
| debug info | on | off | **on** |

`opt-level = 2` rather than 3 because that is what every profile in this tree
that states one already picks, and because the root manifest's own
`[profile.release] opt-level = 2` says the project has already made that choice
once.

`debug = true` produces nothing today (§2.3) — `toyos-ld` drops it. It stays for
two reasons and neither is the artifact: the intent is recorded where the next
reader looks, and the day `toyos-ld` keeps `.debug_line` the profile does not
have to be revisited. Its cost is compile time, unmeasured (§8).

**Built as `[profile.toyos]`**, declared by every crate root the image is made
of — kernel, bootloader, the userland workspace, `toyos-cc`, `toyos-ld`, and
`tests/toyos-rust-tests` with its four subcrates — and passed as
`--profile toyos` by `build::cargo_build`, which is the only place this build
system invokes cargo for a guest target. `--release` is gone from `cargo run`,
from `build()`, and from `build_and_assemble`; there is no second profile left
to pick, which is why there is no longer a flag. The per-package overrides in
`userland/Cargo.toml` go with it: one opt-level for the whole image needs none.

### 6.1 What it measured

Image, whole: **729,808,896 → 701,497,344 bytes**, −28,311,552 (−3.88%). All of
it in the binaries; the toolchain does not move because it is the sysroot's
build and not ours.

| | before | after | |
|---|---|---|---|
| initrd `bin/` group | 89,985,665 | 64,425,193 | **−28.4%** |
| `bin/toyos-cc` | 32,977,320 | 14,957,688 | −54.6% |
| `bin/toyos-ld` | 6,827,984 | 3,402,328 | −50.2% |
| `bin/sshd` | 12,245,032 | 10,311,136 | −15.8% |
| `bin/shell` | 1,993,584 | 1,777,760 | −10.8% |
| `bin/toybox` | 2,440,160 | 2,173,776 | −10.9% |
| `bin/terminal` | 2,051,240 | 1,904,704 | −7.1% |
| `BOOTx64.EFI` | 946,176 | 513,024 | −45.8% |

The two halves of the table are the two halves of §1.4's inconsistency:
`toyos-cc`, `toyos-ld`, the bootloader and every unlisted userland program were
at opt-level 0 and roughly halve; the compositor and doom were already at 2 and
barely move. The kernel is not comparable across this change — `main` gained the
HDA driver between the two builds.

The checks are not free and the figure is the honest one: the same kernel is
**3,784,872 bytes with `overflow-checks` on and 3,296,672 with it off**, so
14.8% of the kernel is the checks. That is the price of the two crafted-ELF
panics being findable.

## 7. Gates

Each stage lands with a gate that is watched red before the fix and green after.

**Stage 1 — a named backtrace for a program running from disk.** *Built.*
`disk_backtrace` copies its child onto `/home` and runs it from there;
`check_disk_backtrace` asserts the SEGFAULT report names
`null_deref_run_from_disk`, a symbol no other binary in the boot has. Negative
control watched: with `read_backtrace_table` stubbed to `empty_with_bounds`, the
report reads `0x1000000102a  [exe+0x102a]` and the test is red — twice, because
the harness re-runs a red from the wide phase alone. Green after, with
`0x10000001174  disk_backtrace_child::null_deref_run_from_disk+0x14`.

**Stage 2 — every existing test.** The suite boots ~76 machines and every one of
them mounts the initrd as `/`. Moving that mount to a partition is exercised by
the whole suite by construction. The gate that is *not* automatic is the one for
the failure policy: a boot whose root partition GUID is absent must say so by
name rather than wedge.

**Stage 3 — the image is smaller and the system still boots.** Plus a gate that
`hosted-rustc = true` still assembles, since the code path stays.

**Stage 4 — overflow checks are on in the shipped kernel.** *Built, and it is not
a test.* `build::assert_overflow_checked` searches the staged kernel for
`attempt to add with overflow` — the panic message rustc references beside every
checked add, which the linker's liveness pass drops when there is no call site —
and refuses to write an image without it. In `build()` and in
`build_test_image`, so `--build-only`, `--diag-boot`, `--console-boot` and every
harness boot are all covered by the same check. Negative control watched: with
`overflow-checks = false` in `kernel/Cargo.toml`, `cargo run -- --build-only`
stops with *"the kernel was built without overflow checks: nothing in 3,717,296
bytes references …"*.

A kernel feature that overflows a `u8` would have cost a boot and proved less:
it would say the checks are on *in the kernel that test booted*, where this says
it about every image this build system writes.

## 8. Measurements still to take

- **The four-way split of a boot's wall clock** on current `main`: firmware,
  initrd read, kernel phases, userland. The 2026-07-29 document cited 4.96 s /
  2.48 s / 1.42 s / 0.93 s from `specs/arm64-research-2026-07-28.md` §1, taken
  before `019e963` deleted the bootloader's 635 MiB memset and before the kernel
  was built with optimisation on. **Every derived timing in the old document
  scaled off that rate and none of it is carried forward here.** Nothing in this
  session timed a boot: the shared sysroot was held by another worktree for the
  first two hours, and after that every measurement here was a size.
- **Bootloader instrumentation** (the old §5.1 / E1): `rdtsc` deltas around the
  pool allocation, `file.read`, `load_kernel_elf` and `exit_boot_services`. Ten
  lines. It is what makes every later change attributable, and it gives the
  per-byte rate twice at two very different sizes in one boot.
- **What `debug = true` costs in compile time**, since §2.3 shows it buys no
  artifact.
- **What stage 1 costs at spawn.** `spawn:` now prints `symbols=NKiB` per
  process — every binary in a test boot came out at 2048 KiB, which is
  `PageAlloc`'s one-page minimum and not a measurement of the tables — and the
  phase breakdown beside it does not yet separate the symbol read from `layout`.

## 9. What was priced and rejected

**Compressing the initrd.** Retained from the 2026-07-29 document, whose ratios
were measured on this host: zstd -3 4.27x, gzip -6 3.84x, lz4 -1 2.72x on
`librustc_driver`. Rejected because decompression runs in the guest under TCG
against a read path that is not obviously slower, because it doubles peak memory,
because it adds a decompressor to the bootloader, and above all because it
addresses the transfer and not the occupancy. Stage 2 addresses the occupancy and
makes the whole question moot.

**Right-sizing the bcachefs volume** (the old §5.4 / R3): 61,379,681 bytes,
9.13% of the initrd volume, is `create_initrd`'s `* 11 / 10` safety margin,
materialised by `VecBlockIO::new`. Worth doing if the initrd survives. Stage 2
deletes the initrd, and a root partition is sized by the same estimator — so the
work moves rather than disappearing, and it is a `bcachefs` change (re-place the
backup superblock, rewrite `block_count`) rather than a `truncate`. Not in this
plan's four stages; recorded so it is not lost.

**Swapping the dev loop to `virtio-blk`** (the old §5.3 / R2): rejected outright,
see §3.2.
