# Boot image split — investigation and design

> 2026-07-29. Design and research only; nothing in this document has been
> implemented and no measurement in it required a build. Every size is read off
> this working tree; every timing is either cited from
> `specs/arm64-research-2026-07-28.md` §1 or explicitly marked estimated or
> `[unverified]`. The tree was in use by other agents throughout, so the two
> experiments this document most wants — §4.4 — were not run. They are specified
> precisely enough to run in one sitting.

## 0. What this decides, and what it refuses to decide

The owner proposed splitting the boot image into two filesystems: an initrd, and
a second store mounted once a USB storage driver exists. The premise was that
half the boot wait is one enormous initrd. **The premise is true and the size
accounting confirms it exactly.** The recommendation is nevertheless *not* to
build the split first, because three changes that cost between five lines and
fifty each are ahead of it, one of them prices the split without writing any of
it, and the split's own cost — a USB mass-storage driver — is the largest single
item on the list by an order of magnitude.

What survives review, in order:

1. **Instrument the bootloader** (§5.1). Nobody has attributed the 2.48 s.
2. **Stop zeroing 635 MiB you are about to overwrite** (§5.2). Verified defect,
   ours, five lines — and **already fixed in the working tree** by concurrent
   work while this document was being written; see §5.2.
3. **A/B the emulated boot medium** (§4.4, §5.3). Four lines, dev-loop only,
   and it is also the experiment that answers "size or I/O pattern".
4. **Stop shipping 58.02 MiB of padding** (§5.4). Measured exactly.
5. **Then re-measure, and decide the split on the memory argument rather than
   the boot-time one** (§6, §7). After 1–4 the boot-time case may be gone; the
   memory case — 635 MiB of a 2 GiB guest, permanently reserved, 83% of it a
   toolchain most boots never invoke — survives regardless.

And one thing this document refuses to decide: whether a USB mass-storage driver
belongs in the kernel at all. §3.4 sets that out as a genuine architectural fork
that is the owner's call, not a detail of this design.

## 1. What is actually in the 635 MiB

### 1.1 The image, measured

`target/bootable.img` is 676,941,784 bytes (built 2026-07-29 22:34). Parsing its
GPT and FAT32 directly:

| Path in ESP | Bytes | MiB |
|---|---|---|
| `/EFI/BOOT/BOOTX64.EFI` | 915,968 | 0.87 |
| `/TOYOS/KERNEL.ELF` | 5,649,880 | 5.39 |
| `/TOYOS/INITRD.IMG` | **666,079,232** | **635.22** |

One GPT partition, `EFI System`, LBA 34..1321985 (645.5 MiB), FAT32 with 4096-byte
clusters. The initrd is 98.4% of the ESP and 98.4% of everything the bootloader
reads.

### 1.2 The content, and why the accounting can be trusted

`src/image.rs:12-19` sizes the bcachefs image from the content:

```
data_blocks  = ceil(data_size / 4096)
btree_blocks = max(total_entries / 30, 2)
total_blocks = (1 + 64 + btree_blocks + data_blocks) * 11 / 10
```

Replicating `build_and_assemble` (`src/build.rs:209-312`) and `collect_hosted_rustc`
(`src/build.rs:546-603`) file-by-file against this tree gives 110 files plus 16
symlinks, `data_size` = 605,244,778 bytes, and therefore `total_blocks` = 162,617,
i.e. **666,079,232 bytes — the measured size of `INITRD.IMG` to the byte.** The
breakdown below is not an estimate; it reproduces the artifact.

| Group | Files | Bytes | MiB | % of content |
|---|---|---|---|---|
| Hosted rustc — `lib/*.so` | 18 | 344,889,056 | 328.91 | 56.98 |
| Hosted rustc — `lib/rustlib/.../lib/*.rlib,*.rmeta` | 60 | 135,808,606 | 129.52 | 22.44 |
| Hosted rustc — cranelift backend | 1 | 20,867,616 | 19.90 | 3.45 |
| Hosted rustc — `bin/rustc` shim | 1 | 18,240 | 0.02 | 0.00 |
| **Hosted rustc, total** | **80** | **501,583,518** | **478.35** | **82.87** |
| Userland binaries (`bin/*`) | 17 | 87,192,088 | 83.15 | 14.41 |
| Assets (`share/*`) | 13 | 16,469,172 | 15.71 | 2.72 |
| **Content total** | **110** | **605,244,778** | **577.21** | **100** |
| bcachefs slack + metadata | — | 60,834,454 | 58.02 | — |
| **Image** | | **666,079,232** | **635.22** | |

Largest individual entries:

| MiB | Entry |
|---|---|
| 198.33 | `lib/librustc_driver-27f9f45c02407b54.so` |
| 60.37 | `lib/rustlib/x86_64-unknown-toyos/lib/libcore-*.rmeta` |
| 31.43 | `bin/toyos-cc` |
| 19.90 | `librustc_codegen_cranelift-1.96.0-dev.so` |
| 11.62 | `bin/sshd` |
| 10.69 | `lib/librustc_macros-*.so` |
| 10.03 | `lib/rustlib/.../libstd-*.rlib` |
| 5.93 | `share/wallpaper.rgb` |
| 5.72 | `share/timgm6mb.sf2` |
| 6.13 | `bin/toyos-ld` |
| 4.82 | `bin/files` |
| 4.79 | `bin/compositor` |
| 4.00 | `share/doom1.wad` |

### 1.3 Four observations that fall straight out of the table

**(a) The toolchain is 82.87% of the content and 75.30% of the image.** Nothing
else is close. Any conversation about the initrd's size is a conversation about
`hosted-rustc = true` in `system.toml`.

**(b) 58.02 MiB — 9.13% of the image — is allocator slack.** `create_initrd`'s
`* 11 / 10` is a safety margin on an estimate, and `VecBlockIO::new` allocates
the whole thing (`bcachefs/src/block_io.rs:81-86`) whether or not it is used.
Those bytes are read off the boot medium on every boot. Note the slack is not
purely trailing zeros that could be truncated blindly: `bcachefs/src/fs.rs:282`
lays the volume out as `[superblock(1)] [bitmap] [journal] [data...] [sb_backup(1)]`
and `superblock.rs:111,125` puts a backup superblock at the *last* block. Trimming
means re-placing that block and rewriting `block_count`, not `truncate()`.

**(c) Everything is an unoptimized, unstripped debug build.** `cargo run` passes
no `--release` (`src/main.rs:50`, `src/build.rs:317-318`), so `bin/toyos-cc` is
31.43 MiB and `bin/sshd` 11.62 MiB. This is deliberate and load-bearing — see
§2.3 — but it means tier 1 is roughly 4x larger than it needs to be if the
symbolication requirement were met another way.

**(d) One asset expands 18.5x at build time.** `assets/wallpaper.jpg` is 336,669
bytes on disk; `src/assets.rs:101-115` decodes it to raw RGB and stores
`share/wallpaper.rgb` at 6,220,808 bytes. That is a defensible trade (no JPEG
decoder in userland) but it is 5.93 MiB of boot I/O, ~23 ms at the current rate,
to save a decode. Worth knowing; not worth acting on before §5.

### 1.4 Is anything redundant or accidentally included?

Nothing accidental was found. Two things are arguably misplaced rather than
accidental:

- `bin/toyos-cc` (31.43 MiB) and `bin/toyos-ld` (6.13 MiB) are development tools
  in the same tier as the compositor. Together they are 37.56 MiB — 45% of all
  userland binaries — and neither is in the init sequence.
- `share/timgm6mb.sf2` (5.72 MiB) and `share/doom1.wad` (4.00 MiB) exist for one
  program (`bin/doom`, 3.68 MiB). 13.40 MiB of the image is Doom.

Both are natural tier-2 members and neither is why the image is 635 MiB.

## 2. What the kernel genuinely needs before a storage driver exists

### 2.1 The init sequence, traced

`kernel/src/main.rs:191-363`, seven phases:

| Phase | Does | Needs the initrd? |
|---|---|---|
| 1 Memory | `mm::init`, reserves the initrd region | No |
| 2 CPU | MADT, LAPIC, IDT, syscalls, kernel symbols, HPET, timer | No |
| 3 **Storage** | ECAM, PCI enumerate, **NVMe init**, page cache, mount bcachefs | No |
| 4 Peripherals | **xHCI init** (HID only), ACPI power | No |
| 5 Subsystems | SMP, VFS, process, scheduler, pipes, io_uring, listeners, shm; **mount initrd as `/`**; mount NVMe at `/home`; tmpfs at `/tmp` | Yes, here |
| 6 Devices | virtio console, net, sound, GPU | No |
| 7 Userland | spawn each entry of `init_programs` | Yes |

Two facts matter for the design. **NVMe and PCI are already up in Phase 3, two
phases before the initrd is mounted** — so "a block device before the root
filesystem" is not a new shape for this kernel, it is the existing shape.
And **xHCI is already up in Phase 4**, before the mount, so a mass-storage
driver would have a natural home without reordering anything.

### 2.2 The tier-1 binary list

`system.toml`'s init list is exactly:

```
/bin/toybox locale --load     2.04 MiB
/bin/compositor               4.79 MiB
/bin/soundd                   2.02 MiB
/bin/netd                     2.06 MiB
/bin/sshd                    11.62 MiB
```

plus, spawned by the compositor at runtime (`userland/compositor/src/main.rs:733,
898, 1106`): `/bin/filepicker`, `/bin/terminal`, and whatever `LAUNCHER_APPS`
names. The shell sets `PATH=/bin` if unset (`userland/shell/src/main.rs:13-14`).

Assets tier 1 needs: `share/fonts/JetBrainsMono-Regular-8x16.font` (0.05 MiB) and
`share/wallpaper.rgb` (5.93 MiB).

**Upper bound on the working set at boot: five binaries plus two assets = 28.51
MiB, 4.5% of the image** — and that is an upper bound, because
`insert_elf_regions` (`kernel/src/loader.rs:628`) demand-pages segments through
`FileBacking::read_page`, so only touched pages are ever copied. The other
606.71 MiB is loaded off the boot medium, occupies RAM for the machine's whole
life, and is never read on a boot that does not invoke rustc.

### 2.3 The one thing that would degrade

`kernel/src/loader.rs` is written entirely against `FileBacking` and uses
`read_file_range` everywhere — running a program from a block device already
works, and is how `/home` works today. The single exception is
`process.rs:1482-1537` `find_symtab_in_memory`, whose doc comment says "return
pointers into the initrd memory. No allocation — the sections are read
in-place", and which calls `FileBacking::memory_ptr`. That method is
`None` by default and implemented only by `InitrdBacking`
(`kernel/src/file_backing.rs:22-24, 129-139`).

So: **a tier-2 program keeps working; its user backtraces lose symbol names.**
`find_symtab_in_memory` already returns `empty()` on that path, so the
degradation is graceful and pre-existing rather than new. It is also the reason
(c) in §1.3 matters less than it looks: the debug symbols in tier-2 binaries are
already unusable the moment they leave RAM.

## 3. Is there a USB storage driver? No, and this is the expensive part

### 3.1 What exists

`kernel/src/drivers/xhci/` is 1,029 lines across three files and is a
**HID-only** driver. `scan_ports` → `init_device` addresses every connected
device, then `parse_hid_config` (`device.rs:97-166`) accepts only
`bInterfaceClass == 3` and returns `None` otherwise — logging
`"no HID boot interface found, skipping"`. The QEMU USB stick is enumerated,
given a slot, addressed, and then discarded.

Reusable as-is: `TrbRing::enqueue` with link-TRB wrap, the command ring, the
event ring and `advance_event_ring`, the `Configure Endpoint` command,
`control_transfer`, MSI-X wiring.

### 3.2 What is missing, specifically

1. **Bulk endpoints.** Transfer rings come from a fixed DMA layout —
   `OFF_KB_INT_RING` and `OFF_MOUSE_INT_RING`, two of them, at hardcoded offsets
   in a 0xD000-byte pool (`mod.rs:151-164`). There is no per-endpoint ring
   allocator. Mass storage needs two more rings per device (bulk IN + bulk OUT).
2. **Slots.** `device.rs:87-94` `output_ctx_offset` **panics past three devices**
   — three fixed output-context offsets. Today's dev loop has exactly three USB
   devices (stick, keyboard, tablet). Adding a real storage device to that budget
   means replacing the fixed mapping with a proper DCBAA.
3. **Data buffers.** `OFF_DATA_BUF` is a single shared 4 KiB page and transfers
   name fixed physical offsets into it. Bulk reads must land in caller-supplied
   buffers, with TRB chaining across 64 KiB boundaries.
4. **Descriptor parsing** for class 8 / subclass 6 / protocol 0x50, and the two
   bulk endpoint descriptors.
5. **Bulk-Only Transport.** CBW/data/CSW, tag matching, residue, STALL recovery
   via CLEAR_FEATURE(HALT), Bulk-Only Mass Storage Reset, GET_MAX_LUN.
6. **SCSI.** INQUIRY, TEST UNIT READY, REQUEST SENSE, READ CAPACITY(10)/(16),
   READ(10)/(16), and the unit-attention retry dance every real stick requires.
7. **Concurrency.** Every transfer today is a polled spin (`wait_command`,
   `mod.rs:224`; `wait_transfer`, `mod.rs:270`) executed serially at boot, and
   `wait_command` discards transfer events it happens to dequeue. That is
   correct for one-shot enumeration and completely wrong once storage transfers
   race HID interrupts. A driver used after boot must block and wake through the
   scheduler. `XHCI` is a spinning ticket lock taken from every scheduler pass
   (`drain_irqs`) and from the `fd.rs` read path; holding it across a
   multi-millisecond bulk transfer stalls every CPU that wants it.
8. **A second block device.** `kernel/src/page_cache.rs:11-12` holds exactly one
   `BLOCK_DEV: Lock<Option<Box<dyn BlockDevice>>>`. `block.rs:2` defines a
   `DeviceId`, and the page cache stores it — as `_device_id`
   (`page_cache.rs:97`), read nowhere — because there has only ever been one
   device. Mounting a second filesystem from a second device requires making the
   page cache multi-device.

### 3.3 Honest size

For scale: `drivers/nvme.rs` is a complete NVMe driver in **421 lines**, because
NVMe's device model is a submission queue and a completion queue. USB is not
that. Items 1–3 are a rewrite of the existing driver's resource management, not
an addition to it; 5–6 are new protocol layers; 7 is scheduler integration; 8 is
a page-cache change.

**Estimate: 1,200–2,000 lines of new or rewritten kernel code, of which roughly
400 is rewriting code that works today.** That is an estimate, not a
measurement. It is between three and five times the entire NVMe driver, and it
is the single largest item anywhere in this document.

### 3.4 The architectural fork nobody can duck

CLAUDE.md: *"Kernel — Minimal. New additions to the kernel must be discussed and
justified."* And the daemon model is explicit: compositor, netd, soundd, sshd
each claim a device from the kernel and drive it in userspace.

By that model a USB mass-storage driver belongs in **userspace**. But the VFS is
in the kernel (`kernel/src/vfs.rs`), mounts take `Box<dyn FileSystem>`, and there
is no mechanism for a kernel mount point backed by a userspace daemon. So the
choice is:

- **(A) Kernel driver.** Consistent with NVMe and with today's VFS. Adds
  1,200–2,000 lines to a kernel whose stated direction is *smaller*.
- **(B) Userspace `storaged` + a kernel VFS backend that calls out to it.** The
  right shape under the daemon model and under `capability-handles-spec.md`.
  Costs the driver *plus* an upcall path the kernel does not have, and puts a
  userspace round trip on every page fault against `/usr`.

This document does not choose. It notes that (A) is what the tree's existing
shape asks for, that (B) is what its stated direction asks for, and that
committing to (A) to save 2 seconds of boot is a poor reason to grow the kernel
— which is most of why §7 puts the split last.

## 4. Size, or I/O pattern? — the crux

### 4.1 The rate

From `specs/arm64-research-2026-07-28.md` §1 (measured there, not here): 4.96 s
host wall clock to "Boot: complete", of which 2.48 s is the initrd load, 1.42 s
OVMF, 0.93 s the ToyOS kernel.

666,079,232 bytes / 2.48 s = **268.6 MB/s = 256.1 MiB/s**. Every derived timing
in this document uses 256.1 MiB/s and inherits that measurement's uncertainty.

**That baseline is already stale on two counts**, both from commits that landed
in this tree on 2026-07-29 while this document was being written: `019e963`
removed the memset (§5.2), which was inside the 2.48 s, and `1c441ad` — "Build
the kernel with optimisation on" — changes the 0.93 s kernel phase and possibly
the whole shape of the 4.96 s. **The first thing anyone acting on §7 should do
is re-measure the four-way split of a boot on current HEAD.** That is E1, and it
is now worth more than it was when it was written.

### 4.2 On our side of the boundary there is nothing left to batch

`bootloader/src/main.rs:36-66` `load_file_bytes` issues **one**
`EFI_FILE_PROTOCOL.Read` for the entire file (`main.rs:63`). There is no loop,
no chunk size, no buffer reuse. Everything below that call is OVMF: FatDxe →
DiskIo → BlockIo (UsbMassStorageDxe) → UsbBusDxe → XhciDxe → QEMU's
`usb-storage` on `nec-usb-xhci`. Whatever chunking exists lives in firmware this
project does not build and cannot change.

That matters for how the question is posed. Two mechanisms explain a flat
256 MiB/s and the rate alone cannot tell them apart:

- **(a) per-byte** — emulated DMA and memcpy throughput; time ∝ bytes.
- **(b) per-transaction** — a fixed chunk size somewhere in the EDK2 stack (the
  usual suspect is `USB_BOOT_IO_BLOCKS` in `UsbMassBoot.h`, which would put a
  cap on how many blocks one SCSI READ(10) carries) `[unverified — the OVMF
  source is not in this tree and the constant was not read]`; time ∝ bytes as
  well, because the chunk count is proportional to size.

**Under both, halving the bytes halves the time.** For the firmware half, "size
or I/O pattern" is a distinction without a consequence: we cannot make the
transactions larger, so bytes is the only lever.

### 4.3 But a third component is ours, and it is not a read at all

`bootloader/src/main.rs:62`:

```rust
let mut bytes = vec![0; size];
file.read(&mut bytes).expect("Failed to read file");
```

That zeroes 635 MiB and then immediately overwrites all of it. The chain is
verified in this tree, not assumed:

- `vec![0u8; n]` → `impl SpecFromElem for u8`, `elem == 0` branch →
  `RawVec::with_capacity_zeroed_in` — `rust/library/alloc/src/vec/spec_from_elem.rs:47-52`.
- → `alloc_zeroed` on the global allocator. uefi 0.26's `Allocator`
  (`~/.cargo/registry/.../uefi-0.26.0/src/allocator.rs`) implements `alloc` and
  `dealloc` **and nothing else** — no `alloc_zeroed` override.
- → the default `GlobalAlloc::alloc_zeroed`,
  `rust/library/core/src/alloc/global.rs:216-226`: `self.alloc(layout)` followed
  by `ptr::write_bytes(ptr, 0, size)`.

So the bootloader touched the buffer twice: 635 MiB of `memset`, then 635 MiB of
file data. The `memset` ran under TCG, on the vCPU thread, and bought nothing —
`file.read` overwrote every byte of it. **This half was an ours-to-fix work
pattern, and it was free.** What fraction of the 2.48 s it was: `[unverified]`.
It has since been fixed — §5.2.

(The other `alloc_zeroed` in the file, `alloc_kernel_memory` at `main.rs:21-27`,
is legitimate — an ELF's BSS must be zero — and is 12.4 MiB, not 635.)

### 4.4 The two experiments that settle it

Neither was run: the tree was in use and both require a build and a boot.

**E1 — attribute the 2.48 s.** Add `rdtsc` deltas in the bootloader around (i)
the `allocate_pool` + memset, (ii) `file.read`, (iii) the kernel's own load,
(iv) `load_kernel_elf`, (v) `exit_boot_services`, and print them. ~10 lines,
thrown away afterwards or kept behind a feature. This splits §4.2 from §4.3 and
makes every later change attributable. It also gives the per-byte rate twice at
two very different sizes (5.39 MiB kernel, 635.22 MiB initrd) in a single boot,
which is exactly the fixed-vs-proportional discriminator.

**E2 — swap the emulated boot medium.** `src/qemu.rs:30-33` and
`tests/common/qemu.rs:313-319` attach the image as
`usb-storage,bus=xhci.0,drive=stick,bootindex=0`. Replace with
`virtio-blk-pci,drive=stick,bootindex=0` and re-measure. VirtioBlkDxe issues one
virtqueue request per `BlockIo` call and QEMU services it with a host `preadv`;
if the 2.48 s is per-transaction cost in the USB stack, this collapses it, and
if it is memset-bound it barely moves. Either outcome is decisive.

E2 is safe and independently worth doing regardless of its timing result:

- The ToyOS bootloader uses the UEFI Simple File System protocol and does not
  know or care what bus the ESP is on. Real hardware is unaffected — you still
  boot from the stick, and the machine's own firmware reads it however it reads
  it.
- Nothing in the tree depends on the stick being on the xHCI bus. `grep` across
  `tests/` finds USB only in the two QEMU command lines.
- It **frees an xHCI slot**. §3.2 item 2: `output_ctx_offset` panics past three
  devices, and the stick is currently one of exactly three.
- It removes a device the ToyOS xHCI driver enumerates, addresses, and then
  throws away on every boot.

The one thing to check when doing it: OVMF must carry VirtioBlkDxe (standard
OVMF builds do) and `bootindex=0` must move with the device.

### 4.5 Answering the question as posed

**Provisionally: size, with a real and separable I/O-pattern component that is
ours.** The firmware half is proportional to bytes under either mechanism and we
have no lever on it but bytes. The bootloader half — a 635 MiB memset that
buys nothing — is pure pattern and is free to delete. E1 and E2 turn
"provisionally" into a number. Do not act on the ordering in §7 past step 3
without them.

## 5. The cheap fixes, in detail

### 5.1 R0 — Instrument the bootloader

E1 above. No behaviour change. This is the "always be empirical" rule applied to
a 2.48 s number that has never been decomposed.

### 5.2 R1 — Delete the 635 MiB memset — **LANDED as `019e963`**

This was written as a proposal and landed in `bootloader/src/main.rs` by
concurrent work in this tree before the document was finished — `019e963`,
"Stop zeroing 635 MiB of initrd immediately before overwriting it". It is
recorded here rather than rewritten, because the analysis in §4.3 stands on its
own and the fix arrived at it independently: the commit message traces the same
`SpecFromElem` → `alloc_zeroed` → default-`GlobalAlloc` chain, in the same
order, from the same three sources.

What landed: a new `alloc_uninit(size)` using `alloc::alloc::alloc` +
`Vec::from_raw_parts` — the `alloc_kernel_memory` idiom already present at
`main.rs:21-27`, so no new pattern enters the file — plus
`assert_eq!(read, size)` on `File::read`'s return. That assert is the part worth
noting: once the buffer is not zeroed, a short read leaves allocator garbage in
the tail and the caller parses it as image content, so the length check is not
belt-and-braces, it is what makes removing the memset safe.

Still open, and unchanged by the fix: **how much of the 2.48 s it was.** The
saving is `[unverified]`, upper-bounded by the memset's own cost, and E1 (§4.4)
is still the way to find out — now as an A/B against the parent commit rather
than as an attribution of a single boot.

Not touched, and correctly so: `file_info_len` at `main.rs:56` is the same
pattern at a few hundred bytes, and `alloc_kernel_memory` must keep zeroing
because an ELF's BSS depends on it.

### 5.3 R2 — Swap the dev-loop boot medium

E2 above. Four lines across `src/qemu.rs` and `tests/common/qemu.rs`. Dev-loop
and test-harness only; no guest code changes; real hardware unaffected. Saving:
`[unverified]`, potentially most of the 2.48 s, potentially near zero. Worth
landing on the xHCI-slot argument alone.

### 5.4 R3 — Stop shipping 58.02 MiB of padding

`src/image.rs:14-19` pads the volume 10% and `VecBlockIO::new` materialises all
of it. **Measured: 60,834,454 bytes = 58.02 MiB = 9.13% of the image.** At
256.1 MiB/s that is **0.227 s** (estimated saving; measured input).

The fix is not `truncate()` — see §1.3(b). It is a small `bcachefs` API: after
`create`/`create_symlink` are done, shrink `block_count` to the highest block
actually allocated, rewrite the superblock, and re-place the backup superblock
at the new last block. The read-only kernel mount reads `block_count`,
`bitmap_start` and `bitmap_blocks` from the superblock, so an oversized bitmap
left behind is harmless as long as those three stay self-consistent — that is
the invariant to test.

One trap: **`Superblock::next_alloc` is not a high-water mark.** It is a
wrapping allocation cursor — `alloc_bitmap.rs:47-48` moves it *backwards* when a
block is freed and `:177-179` wraps it to 0 at the end of the device. On a fresh
mkfs that never frees anything it happens to equal the high-water mark, which is
exactly the kind of coincidence that survives review and then breaks. Take the
maximum from the bitmap, or track it explicitly.

Alternatively: iterate the estimate. Format once, read the high-water mark,
format again at exactly that size. Simpler, costs a second build-time pass over
577 MiB, and the build system is not the bottleneck.

### 5.5 R4 — The zero-line experiment that prices the entire split

`system.toml:10` is `hosted-rustc = true`. Setting it to `false` removes
501,583,518 bytes and takes the image from 635.22 MiB to **109.03 MiB** —
computed with the same model that reproduced the real artifact to the byte.
Booting that measures, today, with no new code, exactly the boot-time benefit
that a two-tier split with a working USB driver would eventually deliver.

This is not a proposal to ship `hosted-rustc = false` — the owner has ruled that
out, correctly: `cargo run` must be a fully fledged environment with the Rust
compiler intact. It is a proposal to **find out what the split is worth before
building it.** `cargo test` already runs this way (`tests/testcases/system.toml`
has no `hosted-rustc` key, and the recorded test initrd is 189,267,968 bytes),
so the configuration is already exercised.

Projected at 256.1 MiB/s, if E1/E2 leave the rate unchanged:

| Split | Image | Load | Saved |
|---|---|---|---|
| today | 635.22 MiB | 2.48 s | — |
| minus rustc | 109.03 MiB | 0.43 s | **2.06 s** |
| minus rustc, `toyos-cc`, `toyos-ld` | 67.71 MiB | 0.26 s | 2.22 s |
| minus rustc, cc/ld, Doom + its assets | 52.96 MiB | 0.21 s | 2.27 s |

Note the shape: **the first line captures 91% of the available saving.** The
long tail of "also move the C compiler, also move Doom" is not worth the tier
bookkeeping.

### 5.6 What was priced and rejected: compressing the initrd

Measured on `librustc_driver-*.so` (207,962,224 bytes, 33% of the whole image),
on this host:

| Codec | Compressed | Ratio |
|---|---|---|
| zstd -3 | 48,754,075 | 4.27x |
| gzip -6 | 54,157,463 | 3.84x |
| gzip -1 | 61,009,166 | 3.41x |
| lz4 -1 | 76,567,446 | 2.72x |

So the payload compresses roughly 3–4x. Rejected anyway, for four reasons of
increasing weight:

1. **Decompression runs in the guest, under TCG, on the vCPU thread.** Against a
   256 MiB/s read path, gzip's decompression is very likely slower than the read
   it replaces; zstd's is comparable; only lz4 is plausibly faster, and lz4 has
   the worst ratio. All three numbers are `[unverified]` under TCG.
2. **It doubles peak memory.** The compressed image and the decompressed image
   are resident simultaneously, in a guest configured with `-m 2G`
   (`src/qemu.rs:20`) that already reserves 635 MiB for the initrd.
3. **It adds a decompressor to a `no_std` UEFI bootloader** — a new dependency
   in the one component that must never be interesting.
4. **It saves nothing that matters.** The bytes still occupy RAM after
   decompression. Compression addresses the transfer and not the occupancy,
   which §6.1 argues is the more durable half of the problem.

The interesting variant — *per-file* compression inside bcachefs, decompressed
lazily by the kernel when a file is actually read — is genuinely better
(the bootloader would read ~180 MiB and the kernel would decompress only the
five files it executes) but it is a real feature in `bcachefs/` plus the kernel
page-fault path, and it directly breaks `InitrdBacking::memory_ptr` and with it
the zero-copy symbol tables of §2.3. It is a larger project than the split and
should not be started before §7's step 5.

## 6. The split, designed

Read this section as conditional on §7 step 5 concluding that the split is still
worth it after R0–R3.

### 6.1 The justification that survives R0–R3

Boot time may not survive. This does:

`kernel/src/main.rs:229-238` passes the initrd to `mm::init` as a reserved
region, and `kernel/src/mm/pmm.rs:219-225` simply *never marks reserved pages
free*. Nothing ever returns them. **635 MiB of a 2 GiB guest — 31% of RAM — is
permanently occupied by an image that is 83% toolchain, on every boot, whether
or not rustc is ever invoked.** CLAUDE.md: *"Never hog resources without
purpose. Free memory when not used."*

That argument is independent of how fast the medium is, and it gets worse as the
toolchain grows. It is the honest reason to split.

### 6.2 Shape

**Tier 2 is its own GPT partition on the same medium, holding a bcachefs image.**

- No new filesystem code. `bcachefs_adapter` already mounts a `BlockIO`
  (`kernel/src/bcachefs_adapter.rs:318-320` for the read-only initrd path,
  `mount()` for the NVMe path). The kernel needs no FAT reader.
- No extent map to smuggle across `exit_boot_services` — a partition has an LBA
  range, and GPT is self-describing.
- `dd`-ing `bootable.img` to a real stick still produces a correct two-partition
  medium. The build system writes one file, as today.
- The bootloader can record the boot device's UEFI device path in `KernelArgs`
  before `exit_boot_services`, so the kernel knows *which* device to look at
  rather than probing. `toyos-abi/src/boot.rs` is already a flat `#[repr(C)]`
  struct of scalars; this is one more field.

**Mounted at `/usr`, in Phase 5**, next to the existing `/home` and `/tmp`
mounts (`main.rs:309-310`) — after xHCI comes up in Phase 4, before Phase 7
spawns userland. Nothing in the phase order needs to move.

**`system.toml` expresses it with a per-program tier, defaulting to tier 1:**

```toml
[programs]
compositor = {}                 # tier 1, unchanged
rustc      = { tier = "usr" }   # tier 2
toyos-cc   = { tier = "usr" }
```

with `hosted-rustc = true` implying tier 2 for the whole sysroot. `SystemConfig`
already has a `ProgramConfig` struct with `#[serde(default)]`
(`src/build.rs:29-35`), so this is one field and one partition of the
`initrd_files` vector in `build_and_assemble`.

### 6.3 Naming and lookup

Tier 1 stays at `/bin` and `/share`. Tier 2 lands at `/usr/bin` and `/usr/lib`.

**Every tier-2 program keeps a symlink in the initrd**: `/bin/rustc -> /usr/bin/rustc`.
`create_symlink` already exists and `system.toml` already has a `[symlinks]`
section, so this is free. Three properties fall out:

- The namespace is complete and stable whether or not tier 2 is mounted. Nothing
  in userland needs to learn about tiers.
- When tier 2 is absent, `open_backing` fails at the symlink target and the
  spawn returns a clear error naming `/usr/bin/rustc` — the system screams at
  the point of use, which is where it is actionable, rather than at boot, where
  it is not.
- Shell completion keeps working: `userland/shell/src/main.rs:1030` reads `PATH`
  and defaults to `/bin`, and the symlinks are in `/bin`. If a second directory
  is wanted, `PATH=/bin:/usr/bin` is the one-line change at `main.rs:13-14`.

### 6.4 Failure policy

The distinction that matters: **a missing tier 2 is an environment fact, not a
kernel bug**, so fail-fast does not mean panicking at boot.

- **Real hardware, tier-2 partition present** — mount it, log it.
- **Real hardware, absent** (recovery medium, a stick imaged with tier 1 only,
  a machine booted from a device the driver does not support) — log a warning
  naming the reason, continue, and let §6.3's symlinks turn it into a precise
  error at the moment someone runs a tier-2 program. The compositor, soundd,
  netd, sshd and the shell all still work.
- **QEMU** — identical, because the dev loop must use the same partition on the
  same virtual medium. This is not negotiable; see §6.5.
- **Genuine kernel bugs on that path** — a corrupt superblock, a partition that
  claims to be bcachefs and is not — panic, as everything else does.

### 6.5 The trap to avoid

If the dev loop obtains tier 2 from a device real hardware does not have — a
second `virtio-blk` disk, say, because it is easy — then **the USB path is
compiled but never executed, and it will ship broken.** That is precisely the
failure `specs/arm64-research-2026-07-28.md` §7 documents in Redox and Theseus:
an interface with one real implementor and one stub that survives review because
nothing forces the stub to run.

So: either the dev loop reads tier 2 through the same USB mass-storage driver
real hardware uses, or there is no split. This raises the cost of the split (the
driver must work well enough to be on the critical path of every `cargo run`, on
day one) and it is the correct constraint.

### 6.6 What this does *not* solve

Tier 2 on the boot medium is what a **live/installer** image needs. An
**installed** ToyOS wants `/usr` on the machine's own disk, which is NVMe, for
which the driver already exists (`drivers/nvme.rs`, 421 lines). The natural end
state is: the stick carries tier 2, and ToyOS installs from it to the internal
disk. The dev loop is permanently a live image, so it permanently needs the
boot-medium driver. Stating this now avoids designing the split as if the NVMe
path were an alternative to it — it is a successor, not a substitute, and per
the brief the build system may not stage into `target/nvme.img` in any case.

## 7. Ordering

Wall-clock savings against the 4.96 s / 2.48 s baseline. "Measured" means read
off this tree or cited from `arm64-research-2026-07-28.md` §1; everything else
is derived at 256.1 MiB/s and inherits that rate's uncertainty.

| # | Change | Cost | Expected saving | Basis |
|---|---|---|---|---|
| 1 | **R0** — `rdtsc` instrumentation in the bootloader (E1) | ~10 lines | 0 s | — |
| 2 | **R1** — delete the 635 MiB memset (§5.2) — **landed** | ~25 lines | unknown; upper-bounded by the memset | `[unverified]`, settled by R0 |
| 3 | **R2** — `usb-storage` → `virtio-blk` in the dev loop and harness (E2) | ~4 lines | unknown; possibly most of 2.48 s | `[unverified]`, settled by measuring |
| 4 | **R3** — right-size the bcachefs image (§5.4) | small `bcachefs` API + `image.rs` | **0.227 s** | measured input, derived saving |
| 5 | **R4-probe** — boot once with `hosted-rustc = false` (§5.5) | one boolean | 0 s (it is a measurement) | prices step 6 exactly |
| 6 | **The split** (§6) | **1,200–2,000 lines of kernel** | ≤ 2.06 s, minus whatever steps 2–4 already took | estimated cost, derived saving |

Steps 1–4 are independently landable, independently revertible, and together
cost under a hundred lines. Step 5 costs one boot. **Do not start step 6 before
step 5 has been run and steps 1–4 have landed**, because steps 2 and 3 may take
most of what step 6 would have taken, and step 5 tells you exactly how much is
left.

If step 5 says the remaining saving is small, the split still has §6.1's memory
argument — 31% of guest RAM, permanently held, 83% of it unused — and that
argument should then be weighed against §3.4's kernel-growth question on its own
terms, not smuggled in behind a boot-time number that no longer holds.

One thing to do regardless of any of this: **R2's slot argument** (§4.4). The
xHCI driver panics at four USB devices and the dev loop runs three, one of which
is a stick the driver enumerates and discards. That is a live constraint on
plugging in any additional USB device, and it costs four lines to remove.

## 8. What could not be verified

- **The 2.48 s decomposition.** Cited from `arm64-research-2026-07-28.md` §1.
  Not re-measured; the tree was in use. Every derived timing here scales off it.
- **The memset's share of the 2.48 s.** The memset is *proved* to happen
  (§4.3, three source citations); its cost under TCG is `[unverified]`. E1.
- **Whether OVMF's USB path chunks, and at what size.** The mechanism is
  plausible and matches the observed rate, but the EDK2 constant was not read —
  the OVMF source is not in this tree, only `ovmf/OVMF_CODE-pure-efi.fd`. E2 is
  a better experiment than reading the constant would be, because it measures
  the consequence rather than the cause.
- **Whether `virtio-blk` is faster here.** Mechanism is strong, number is
  `[unverified]`. E2.
- **Decompression throughput under TCG** (§5.6). The compression *ratios* are
  measured on this host; the decompression side is estimated and is the reason
  compression is rejected rather than a measurement that rejects it.
- **The 1,200–2,000 line estimate for the USB driver** (§3.3). An estimate,
  anchored on `nvme.rs` at 421 lines and `xhci/` at 1,029, not a plan.
- **The tree changed under this document.** Three concurrent changes landed
  while it was being written: `019e963` (the R1 fix, §5.2), `1c441ad` ("Build
  the kernel with optimisation on", which moves the boot baseline — §4.1), and
  an uncommitted edit to `kernel/src/elf.rs`. Every citation was re-checked
  against the working tree afterwards. `elf.rs` is not load-bearing for anything
  claimed here; the other two are, and are called out where they bite.
  **§1's size accounting is unaffected by all three**: it is file sizes rather
  than timings, `019e963` touches only the bootloader, and `1c441ad` touches
  only `kernel/Cargo.toml` — the kernel binary lives at `/TOYOS/KERNEL.ELF` in
  the ESP and is not in the initrd. §4's and §7's *timings* are another matter.
- **`target/bootable.img` and the userland binaries were built at different
  times** (image 22:34, kernel 23:28) by concurrent work in this tree. The
  content accounting in §1.2 nevertheless reproduces the image's size exactly,
  so the two are consistent for the files that matter; the 5,649,880-byte
  `KERNEL.ELF` in the image and the 3,073,392-byte `kernel` on disk are not the
  same build, and neither figure is load-bearing here.
