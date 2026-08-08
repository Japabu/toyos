# Type-safety audit: the USB/boot-volume code that landed after the audit wave

Read-only. Nothing outside this file was changed. The only commands run against
the tree were `cargo test -- usb_storage` (3 passed, 20.0 s) and
`cargo test -- usb_storage_gate --nocapture`, both green; every other number
below is a `grep`, a `wc`, or a standalone `rustc` program in a scratch
directory outside the tree.

Read at HEAD `ad2ca1c` (2026-08-02). **Another agent was editing
`kernel/src/drivers/xhci/` throughout**; every file was re-read immediately
before the finding that cites it, and three findings I had written were deleted
unfiled because that agent landed them first (see "Fixed under me" below).
Per-file commit at the time of reading:

| file | commit |
|---|---|
| `kernel/src/drivers/xhci/device.rs` | `9dfd044` |
| `kernel/src/drivers/xhci/msc.rs` | `9dfd044` |
| `kernel/src/drivers/xhci/mod.rs` | `fdc9cee` |
| `kernel/src/drivers/xhci/legacy.rs` | `9cb9916` |
| `kernel/src/drivers/usb_storage.rs` | `5fde1c5` |
| `kernel/src/usb_gate.rs` | `862741d` |
| `kernel/src/fat32_adapter.rs`, `kernel/src/esp_log.rs` | `5b8565b` |
| `kernel/src/gpt.rs` | `5ddd926` |
| `kernel/src/block.rs` | `3c5a7b8` |

This extends `specs/type-safety-audit/usb-storage.md` (F1–F13) and
`usb-gate-teeth.md`; nothing filed there is re-filed. Where a finding here is
the type-level root of one of theirs it says so.

---

## Summary

**The types in this code are good and they stop one layer short, every time.**
That is the finding the individual items are instances of. `Bot`, `Scsi`,
`Host<'_>`, `Endpoint`, `Walk`, `WalkError`, `Resolution`, `GptError`,
`BlockError` and (in the crate) `Cluster` are all real, and every one of them
was added *after* a defect in exactly the place that defect was. The residual
is therefore predictable rather than random: it is whatever sits one call
outward from the last thing that bit.

Ranked, with the layer each one is outward of:

| # | Finding | Outward of | Bug permitted |
|---|---|---|---|
| F1 | The mass-storage pool block is claimed by `ctrl.storage.len()`, so a `bind` that configures the endpoints and then fails hands the same block to the next disk | `Layout::msc` returning `Option` | A late DMA completion from a device that timed out lands in the next disk's scratch |
| F2 | `Endpoint`'s "one private constructor" invariant is not enforced by its visibility, and both doc comments say it is | `Endpoint::new` | A struct-literal `dci` indexes a 4 KiB DMA region 12,880 bytes in; `1u32 << dci` overflows |
| F3 | `EspBacking::read_page` turns a device error into a page of zeros with **no log line**, and the file cache writes that page back | `BlockDevice`'s new error channel | Silent zeroing of a 4 KiB region of a `/boot` file, from the idle loop |
| F4 | `gpt::probe(dev, lba_bytes)` takes the device's block size as a second argument, from a second lookup | `BlockDevice` hiding the logical block size | Two objects, one fact, paired by convention at two call sites |
| F5 | `with_storage -> Option<R>` never returns `None`; the three `.unwrap_or(false)` are dead, while the two siblings index the same vec directly | `with_disk` | None — two answers to one question in one file |
| F6 | `FLUSH_SENSE` replaces the whole `Scsi` outcome including a broken transport, while its doc says only the CSW's verdict is replaced | `Scsi::Refused` | `usb-flush-unimplemented`'s gate goes green on a boot where the flush never reached the device |
| F7 | `msc::bind -> bool` and the one caller drops it | — | None — dead return |
| F8 | `BlockDevice::read_blocks(lba, count, buf)`: `count` and `buf.len()` are two names for one fact, reconciled by three `assert_eq!`s | `BlockError` | None today; 3 asserts and 16 call sites exist to keep them equal |
| F9 | `EspDevice.len` and `EspVolume.bytes` are the same number in two objects, both narrowed in a second step after `Fat32::probe` | `EspDevice::locate` | None — the claim that makes it safe **holds**, verified |
| F10 | `legacy::take_ownership(bar, bar_size, …)` — `Mmio` knows its own size and will not say | `Mmio::check` | `find`'s "nothing here panics" rests on two numbers agreeing |

**Refutations, stated as prominently as the findings** (detail in Part 3):

- `mount_boot`'s load-bearing claim — *"A boot sector describing more than the
  partition holds is already `Error::Truncated`, so this only ever shrinks"* —
  **holds**. `toyos-fat32/src/boot.rs:185-188` refuses `volume_bytes > capacity`
  before returning a `Geometry`, so `esp.len` can only be narrowed. This was the
  claim most worth breaking and it did not break.
- **`in_dci` and `out_dci` cannot collide.** IN endpoints get odd DCIs and OUT
  endpoints even ones by construction (`num * 2 + is_in`), so
  `1 | (1 << in_dci) | (1 << out_dci)` cannot silently set one bit where it
  meant two. Checked because `bind` would not notice.
- **`transfer_blocks`' `sector_lba as u32` cannot truncate.** Traced end to end:
  `bring_up` refuses `last_lba > u32::MAX`, so `blocks = sectors / spb` gives
  `(blocks - 1) * spb <= 2^32 - spb`, and `lba + count <= dev.blocks` bounds the
  input. Exact, not approximate.
- **`EspBacking::read_page`'s two-`base`-advance loop is correct** over an
  extent list where a page spans two extents, in both the capped and uncapped
  cases. Walked by hand; no off-by-one.
- **`Endpoint`'s `pub(super)` fields do not trip `private_interfaces`.** My first
  reproducer warned; that was an artifact of making the modules `pub`. With the
  real (private `mod msc;`) structure, rustc is silent. There is no lint
  regression to report — the invariant is simply unenforced, quietly.

### Fixed under me — do not act on these, they are already in

`fdc9cee` and `9dfd044` landed, while this audit was in progress, the worked
example the brief opens with (`endpoint_dci` returning `None` for endpoint 0)
*and* the partial-initialization fix I had written up as F1: `Walk` /
`Endpoint` / `Function` now make an incomplete interface unrepresentable, and
the two `!= 0` guards deleted themselves. `f621999`/`5fde1c5` closed
usb-storage.md F1, `5b8565b` closed its adapter half, and `9cb9916` closed F9
and F10. What remains of that shape is F1 and F2 below, both one layer down
from where the fix stopped.

---

## Part 1 — findings

### F1 (high). The mass-storage pool block is claimed by an operation that has not happened yet

**Location.** `msc.rs:626` (`let index = ctrl.storage.len();`), `msc.rs:627`
(`ctrl.layout.msc(index)`), `msc.rs:695-697` (`if !bring_up(…) { return false; }`),
against `mod.rs:368-376` (`Layout::device` / `Layout::msc`).

**The bug the current shape permits.** `bind` picks its DMA block from the
number of disks *already bound*:

```rust
let index = ctrl.storage.len();
let Some(block) = ctrl.layout.msc(index) else { … };
```

and the device only enters `ctrl.storage` at the very end, after `bring_up`.
Between those two points `bind` issues a Configure Endpoint that puts the
device's two bulk endpoints into the Running state with their transfer rings at
`block + MSC_IN_RING` and `block + MSC_OUT_RING`, and then `bring_up` issues
TEST UNIT READY, INQUIRY and READ CAPACITY, whose data phases target
`block + MSC_SCRATCH`.

If `bring_up` gives up — `READY_BUDGET_NS` expiry, a refused INQUIRY, a block
size this driver will not serve, a capacity too large for READ(10) — `bind`
returns `false` **without pushing**, so `storage.len()` is unchanged and the
next disk on the bus gets the same `index` and therefore the same `block`.
`TrbRing::init` zeroes the two ring pages for the newcomer, but the *first*
device's endpoint contexts still name that memory, its slot is still enabled
(the slot leak is filed and deliberate), and a transfer that `wait_transfer`
abandoned on the 2 s deadline is still outstanding on a Running endpoint. Its
late completion writes into the second disk's `MSC_SCRATCH` — which is exactly
where READ CAPACITY's block size and last LBA land — or into its CBW/CSW.

The asymmetry is the tell, and it is inside one `impl`:

```rust
fn device(&self, slot_id: u8) -> Option<usize> { … }   // keyed by a never-reused slot id
fn msc(&self, index: usize) -> Option<usize> { … }     // keyed by Vec::len(), which goes back down
```

HID takes the first and cannot alias: a failed HID bind (`PointerSource::claim`
exhausted at `device.rs:471`) leaks its block forever, which is safe. Mass
storage takes the second.

**Reachability, stated honestly.** Read off the code, not reproduced. Every
`bring_up` failure path is unreachable in QEMU — measured this session over the
full `usb_storage_gate` boot log: **0 occurrences each of `transport broke`,
`reset recovery failed` and `never became ready`**, which is the same absence
`usb-gate-teeth.md` §"QEMU cannot stage these" records. The two-disk half *is*
staged (`msc_block +0x10000` and `+0x20000`, both seen), so only the failing
first bind is missing.

**Proposed shape.** The block is a resource, so claim it where it is taken and
never give it back — the policy the slot already has, and the one `Layout` was
written for:

```rust
pub struct XhciController {
    …
    /// Mass-storage blocks handed out this boot. Monotone, and deliberately not
    /// `storage.len()`: a bind that configures the endpoints and then fails has
    /// pointed the controller at that block, so re-issuing it hands a live
    /// endpoint's DMA target to the next disk. The slot behind it is not given
    /// back either, for the same reason.
    msc_claimed: usize,
}

// bind
let Some(block) = ctrl.layout.msc(ctrl.msc_claimed) else {
    log!("usb-storage: slot {slot_id} is the {}th disk; this driver serves {}",
        ctrl.msc_claimed + 1, super::MSC_BLOCKS);
    return;
};
ctrl.msc_claimed += 1;
```

**What it deletes.** Nothing, and it costs one `usize`. Its argument is the bug,
not the line count. It does remove one double duty: `index` is currently both
the pool-block selector and the input to `ctrl.disk_base + index`, the
machine-wide disk number — which is what the four-line comment at `msc.rs:698-701`
exists to explain. After the change the disk number is `ctrl.disk_base +
ctrl.storage.len()` and the block selector is a different variable, so the two
can no longer be confused by reading.

**A `Drop` guard is the wrong fix here**, per CLAUDE.md's caveat, but not for
the usual reason: the failing path *is* a plain `return` and a guard would fire.
It is wrong because there is nothing safe for it to do — releasing the block is
the bug.

### F2 (high). `Endpoint`'s invariant is documented as enforced by a private constructor, and its visibility does not enforce it

**Location.** `device.rs:34-63` (the type and `Endpoint::new`), `device.rs:22-32`
and `msc.rs:42-45` (the two claims), `msc.rs:633` and `msc.rs:641-666`
(`bind` consuming `dci`), `mod.rs:595-598` (`write_ctx32`).

**The claims.** `device.rs:22`:

> [`Self::new`] is the only way to make one and is private to this module, so a
> `dci` that exists is a device context index the driver may write

and `msc.rs:42`:

> A value of this type cannot describe an interface with one bulk endpoint or
> with an address this driver may not turn into a device context index, because
> [`Endpoint`] has one constructor and it is private to the parser — so `bind`
> has nothing left to check.

**The bug the current shape permits.** `Endpoint` is `pub(super)` with *every
field* `pub(super)`, so any module under `crate::drivers::xhci` — `msc`, `hid`,
`mod` — can build one with a struct literal and never call `new`. Rust requires
only that the struct and all its fields be visible; a private constructor next
to public fields constrains nothing.

**Demonstrated**, with the module structure reproduced faithfully (private
`mod device;` / `mod msc;` under one parent, `pub(super)` struct, `pub(super)`
fields, private `fn new`). It compiles with **zero warnings** under
`rustc --edition 2021`:

```
dci=200 ctx_index=201 byte_offset=12880
out dci=0 ctx_index=1
```

What `bind` then does with those two numbers:

- `msc.rs:641` — `1 | (1u32 << in_dci) | (1u32 << out_dci)`. `kernel/Cargo.toml`
  keeps overflow checks on, so `1u32 << 200` is *attempt to shift left with
  overflow* — a kernel panic during enumeration whose message says nothing about
  USB.
- `msc.rs:655` — `let ctx = dci as usize + 1;` into `write_ctx32`, which is

  ```rust
  fn write_ctx32(&self, ctx_base: *mut u8, slot_index: usize, dword: usize, val: u32) {
      let offset = (slot_index * self.context_size) + (dword * 4);
      unsafe { write_volatile(ctx_base.add(offset) as *mut u32, val); }
  }
  ```

  No bound of any kind, at **23 call sites** (15 in `device.rs`, 8 in `msc.rs`).
  `input_ctx` is `dma.subslice(OFF_INPUT_CTX, PAGE)` — 4096 bytes at pool offset
  16,384. Context index 201, dword 4, at the measured guest's `ctx_size=32`, is
  pool + 22,832: inside `OFF_DATA_BUF` (20,480…24,576), the shared descriptor
  scratch. At the `ctx_size=64` a real xHC reports it is pool + 29,264 — past
  `SHARED_SIZE` (24,576) and into `scratch_array`, the array of physical
  pointers the controller dereferences for its scratchpad buffers. Both numbers
  are computed from the measured boot: `xHCI: max_slots=64 max_ports=8
  ctx_size=32 pagesize=0x1`, `dma 2048 KiB: scratchpad=0`, `msc_block +0x10000`.
  The parameter is also *named* `slot_index` while every caller passes a context
  index (0 is the input control context, 1 the slot context, `dci + 1` an
  endpoint) — the one name the site has is the wrong one.

**Is it live?** No. `Endpoint::new` is the only construction in the tree today
and it yields `dci ∈ 2..=31`, so `ctx <= 32`, `32 * 64 + 16 = 2064 < 4096`, and
`1u32 << 31` is fine. This is a finding about the guarantee, not about today's
behaviour — and the guarantee is exactly what `msc.rs:45`'s "so `bind` has
nothing left to check" spends.

**Proposed shape.** One word:

```rust
pub(super) struct Endpoint {
    pub(super) addr: u8,
    /// 2..=31 by construction. Private, and that is what makes the sentence
    /// above true: a struct literal anywhere under `xhci` would otherwise build
    /// one, and `bind` shifts by this and indexes the input context with it.
    dci: u8,
    pub(super) max_packet: u16,
    pub(super) max_burst: u8,
    pub(super) interval: u8,
}

impl Endpoint {
    pub(super) fn dci(&self) -> u8 { self.dci }
}
```

One private field is enough to make the struct literal impossible outside
`device.rs`; `max_burst` stays `pub(super)` because the SuperSpeed companion arm
(`device.rs:227`) assigns it in place.

**What it deletes.** Three field reads become three calls
(`msc.rs:633` twice, `device.rs:400` once), and **two doc comments become true**
— which is the whole delta, and is worth more than a line count. It also gives
`write_ctx32` its only enforced precondition. The remaining half of that site —
a raw `*mut u8` and an unbounded `usize` — is the type-level root of
`kernel-drivers.md` F6 and F8 (`KernelSlice` destructured into raw halves;
`ptr_at` bounds-checks zero bytes) at 23 sites nobody counted; it belongs with
those, not here.

### F3 (medium). `EspBacking::read_page` serves zeros on a device error and says nothing, and the file cache writes that page back

**Location.** `fat32_adapter.rs:291-322`, specifically `:314`
(`let Some(dev) = guard.as_mut() else { return };`) and `:315`
(`if dev.read_at(…).is_err() { return; }`).

**The bug the current shape permits.** `FileBacking::read_page` returns `()`;
that was filed as the `bcachefs::BlockIO` is infallible entry, now closed and
deleted, whose last paragraph read *"`FileBacking::read_page` and `vfs::FileSystem`'s write path
have no error channel either, so `file_backing.rs` leaves the caller a hole of
zeros **and says so in the log**"*). The new implementation does not say so. The
sibling implementations both do:

```
kernel/src/file_backing.rs:68     log!("file: read of block {block} failed; serving zeros");
kernel/src/bcachefs_adapter.rs:37 log!("bcachefs: read of block {} failed; serving zeros", block.raw());
kernel/src/fat32_adapter.rs:315   if dev.read_at(…).is_err() { return; }     ← nothing
```

So that sentence is now false for one of the three, and the
marker `serving zeros` — which the record itself uses as a grep needle when
triaging (`specs/issues/`, the cache-eviction investigation: *"grep over the
failing run's serial finds zero of `could not be cached`, `serving zeros`, …"*)
— cannot be produced by the ESP path at all. Measured: 0 occurrences in this
session's `usb_storage_gate` boot, and there is no string it could have emitted.

**Why it is worse here than in the two that log it.** It is not only a read.
`file_cache::write_page` (`file_cache.rs:212-218`) fetches the page through the
backing before merging a partial write:

```rust
if page_start < backing.file_size() {
    backing.read_page(page_start, &mut fetched);
}
…
apply_write(file, page_idx, offset, data);
```

`esp_log::Sink::append` writes at `self.size % 4096`, i.e. almost always a
partial write. If that page has been evicted and the re-read fails, `fetched`
stays zero, the new log bytes are merged into it, and `flush_file` writes the
zeroed page back to `/boot/toyos/kernel.log` — **on the volume whose whole
purpose is to be the diagnostic on a machine with no serial port, from the idle
loop, with no line saying so.** `update_metadata`'s own doc
(`fat32_adapter.rs:570-578`) already reasons about "evicting one of those pages
against a stale extent list reads back zeroes" and closes the stale-extent half;
this is the failed-read half of the same sentence.

The device under it is the one device in the machine that can be physically
removed.

**Proposed shape.** The real fix is the trait's error channel and is filed. The
one that costs nothing and restores the record's own instrument:

```rust
if dev.read_at(extent.offset + within, &mut buf[done..done + n]).is_err() {
    // Same line the other two backings print, and the same reason: the caller
    // gets a hole either way, and `serving zeros` is what a triage greps for.
    log!("esp: read of {n} B at volume offset {} failed; serving zeros",
        extent.offset + within);
    return;
}
```

**What it deletes.** Nothing. What it restores is one row of a three-row
invariant that the `specs/issues/` entry asserts and that is currently 2/3 true.

### F4 (medium). The device's logical block size travels beside the device instead of inside it

**Location.** `gpt.rs:100` (`pub fn probe(dev: &mut dyn BlockDevice, lba_bytes: u32)`),
`fat32_adapter.rs:630-635`, `main.rs:365-370`, `nvme.rs:349-355`,
`block.rs:35-53`.

**The shape.** `BlockDevice` deliberately hides the device's logical block size —
`nvme.rs:349-352` writes four lines saying so — so every caller that needs it gets it
from a *second* object keyed the same way:

```rust
// fat32_adapter.rs
for index in 0..usb_storage::count() {
    let Some(mut disk) = usb_storage::open(index) else { continue };
    let Some(geometry) = crate::drivers::xhci::storage_geometry(index) else { continue };
    gpt::probe(&mut disk, geometry.logical_block_bytes);
}

// main.rs
let sector_size = nvme_dev.sector_size();
gpt::probe(&mut nvme_dev, sector_size);
```

Two lookups of one index, in two mechanisms, paired by convention. `open`
already asked `storage_geometry(index)` and threw `logical_block_bytes` away
(`usb_storage.rs:29-36`). The pairing is correct today because both go through
`with_disk(index)`; nothing in the types says it must be.

**The bug it permits.** A `lba_bytes` that does not belong to `dev` makes
`DeviceSectors` read the GPT at the wrong granularity and — because a 512-vs-4096
misread usually fails the `EFI PART` check — report *"device N has no partition
table we can use"* on a perfectly good disk. That is the failure this whole
subsystem exists to make impossible to misdiagnose on a machine whose only
channel is a screen.

**Proposed shape.** The device answers for itself:

```rust
// block.rs
pub trait BlockDevice: Send {
    fn device_id(&self) -> DeviceId;
    fn block_count(&self) -> u64;
    /// The device's own logical block size. Everything above this trait is
    /// written in 4 KiB blocks and does not need it — a GPT is laid out in the
    /// device's blocks and in nothing else, and it is not a separate fact a
    /// caller can pair with the wrong device.
    fn logical_block_bytes(&self) -> u32;
    …
}

// fat32_adapter.rs
for index in 0..usb_storage::count() {
    let Some(mut disk) = usb_storage::open(index) else { continue };
    gpt::probe(&mut disk);
}
```

**What it deletes.** `gpt::probe`'s second parameter and both arguments; the
`storage_geometry` lookup and its `else { continue }` arm in `probe_boot_disks`;
`main.rs`'s `let sector_size = …` line; and `nvme.rs:349-352`'s four-line doc
comment, whose entire subject is why this accessor has to exist outside the
trait. Roughly ten lines, and the two-objects-one-fact pairing goes with them.
`UsbBlockDevice` gains a `lba_bytes` field it can fill at `open` from the
geometry it already reads.

### F5 (medium-low). `with_storage`'s `Option` cannot be `None`, and its two siblings do not pretend otherwise

**Location.** `msc.rs:204-214`, `:219`, `:226`, `:266`; `mod.rs:649-660`,
`:663`, `:668`.

**The shape.** `with_disk` resolves a machine-wide index to a controller and a
local index, and by construction `local < ctrl.storage.len()`:

```rust
if index < first + count {
    return Some(f(ctrl, index - first));
}
```

Two of its three consumers take that at face value:

```rust
with_disk(index, |ctrl, local| ctrl.storage[local].geometry())
with_disk(index, |ctrl, local| ctrl.storage[local].online())
```

The other three go through `with_storage`, which re-checks and cannot fail:

```rust
let mut dev = *self.storage.get(index)?;   // never None
…
Some(out)
```

and are then consumed as `with_disk(…).unwrap_or(false)` wrapping
`with_storage(…).unwrap_or(false)` — two layers of "the disk might not exist",
one of which is real and one of which is not. A `-> Option<T>` that never
returns `None` is worse than no check, because the `.unwrap_or(false)` reads
like a safety net for a nonexistent disk and is dead code.

**Proposed shape**, matching the two siblings:

```rust
/// Run `f` against the `index`-th disk of this controller, writing the
/// device's state back whatever `f` did with it. `index` has already been
/// resolved against `storage.len()` by `with_disk`, which is the only way in.
fn with_storage<R>(&mut self, index: usize, f: impl FnOnce(&mut Self, &mut MscDevice) -> R) -> R {
    let mut dev = self.storage[index];
    let out = f(self, &mut dev);
    self.storage[index] = dev;
    out
}
```

**What it deletes.** The `Option<R>` return, the `?`, the `Some(out)`, and three
`.unwrap_or(false)` — and the file stops giving two answers to one question.

### F6 (medium-low). The flush-sense injection replaces the verdict *and* the transport, while its doc says otherwise

**Location.** `msc.rs:177-183` (`FLUSH_SENSE`), `msc.rs:238-242` (the use).

**The claim** (`msc.rs:166-170`):

> The command is issued either way, so the transport under the injection is the
> shipped transport; only the CSW's verdict is replaced.

**What the code does.**

```rust
let issued = ctrl.scsi(dev, &cdb, 10, 0, 0, false);
let outcome = match FLUSH_SENSE {
    Some((key, asc, ascq)) => Scsi::Refused { key, asc, ascq },
    None => issued,
};
```

`issued` is discarded whole, including `Scsi::Broken`. So under
`usb-flush-unimplemented`, a boot in which the flush's transport actually broke
— `reset_recovery` ran, possibly set `dev.failed`, and the command never reached
the device — still produces `usb-storage: disk N does not implement
SYNCHRONIZE CACHE …` and `msc_flush` still returns `true`. The gate asserting
that line passes on a boot where nothing was flushed and the disk is offline.
This is `usb-gate-teeth.md`'s own rule pointed at the newest actuator: *the
instrument is code, and it will be wrong before the driver is.*

**Proposed shape.** Override an answer, never a silence:

```rust
let outcome = match (FLUSH_SENSE, issued) {
    // Only a device that answered gets its answer replaced. Overriding a broken
    // transport would make the gate green on a boot where the flush never
    // reached the device, which is the thing the gate exists to see.
    (Some((key, asc, ascq)), Scsi::Ok { .. }) => Scsi::Refused { key, asc, ascq },
    (_, other) => other,
};
```

**What it deletes.** Nothing; it makes the doc comment true and gives both gates
a tooth they do not have. Two lines.

### F7 (low). `msc::bind -> bool`, and the one caller drops it

**Location.** `msc.rs:617-624` (`/// Returns whether the device joined
ctrl.storage.` … `-> bool`), `device.rs:389` (statement position, value
discarded).

Rust does not warn: there is no `#[must_use]`, and a discarded `bool` is silent.
Every failure path already logs, so the value carries nothing the caller wants.
Either `#[must_use]` and a use, or:

**What it deletes.** The `-> bool`, the doc line that describes it, the final
`true`, and three `return false` become `return` — four lines and one lie about
what the function is for. This is the same sweep `2c8206e` ("Report results,
never narration") and `36a9bd6` are doing to comments, applied to a signature.

### F8 (low). `count` and `buf.len()` are two names for one fact across the whole block trait

**Location.** `block.rs:39-49`, three `assert_eq!`s (`nvme.rs:363`, `:384`;
`msc.rs:281`), 16 call sites.

The trait documents the contract in prose — *"`buf.len()` must equal `count as
usize * 4096`"*, twice — and the two implementations assert it three times.
Counted over `kernel/src`: **16 call sites, of which 11 pass a literal `1`**, and
the remaining five compute the slice from the same `count` in the same
expression:

```rust
dev.write_blocks(start, count as u32, &buf[..count * 4096])          // page_cache.rs:329
    .read_blocks(block, count as u32, &mut buf[done..end])           // fat32_adapter.rs:200
disk.read_blocks(RUN_START, RUN_LEN, &mut back)                      // usb_gate.rs:179
```

**Proposed shape.** `fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> BlockResult`,
with the count derived and a non-multiple length refused as `BlockError` rather
than asserted. **What it deletes:** the parameter from two trait methods and
four impls, three `assert_eq!`s, the two prose contract lines, and `count as
u32` at five call sites. **What it costs:** one length check per impl, so this
is a wash on check count and a win on "the two operands cannot disagree". Filed
low and honestly: it is trait-wide, it touches `nvme.rs` and `page_cache.rs`
which are outside this scope, and no current call site gets it wrong.

### F9 (low). The volume length lives in two objects and is narrowed in a second step

**Location.** `fat32_adapter.rs:681-708`.

`mount_boot` installs `EspDevice { len: <partition bytes>, … }`, builds
`EspVolume { bytes: len }`, calls `Fat32::probe` through it, and then narrows
*both* separately:

```rust
volume.bytes = volume_bytes;
if let Some(esp) = ESP.lock().as_mut() {
    esp.len = volume_bytes;
}
```

The `if let` has no `else`, so a failure to narrow the clamp is a silent no-op,
and there is a window — the whole of `Fat32::probe` — in which the clamp is the
partition and the filesystem's capacity is the partition too. That window is
necessary (the boot sector is what says how big the volume is), but the
two-object duplication is not: `EspVolume` holds nothing but this number and
`EspDevice` holds it again.

**This one is safe, and the reason it is safe was checked rather than assumed** —
see Part 3. Filed as a readability item only: one `fn narrow(&mut self, bytes:
u64)` on `EspDevice` with `EspVolume::capacity()` reading through `ESP` (which
it already locks for every other method) removes the second copy and the
`if let` with no `else`.

### F10 (low). `Mmio` knows its own size and will not say, so `legacy.rs`'s totality rests on the caller passing it again

**Location.** `legacy.rs:111` (`take_ownership(bar: &Mmio, bar_size: u64, …)`),
`:116-118`, `:131`; `mod.rs:797` (`let bar_size = 0x10000u64;`), against
`mm/mmio.rs:8-34`.

`legacy.rs`'s header is the strongest claim in the scope — *"nothing here
panics, nothing here loops on a number firmware chose, and a list that makes no
sense costs the handoff and never the boot"* — and `find`'s totality rests on
its `read` closure:

```rust
let read = |offset: u64| -> Option<u32> {
    (offset.checked_add(4)? <= bar_size).then(|| bar.read_u32(offset))
};
```

`bar_size` is a `u64` the caller supplies. `Mmio` already holds the authoritative
size in a private field and exposes no accessor, so `init_one` writes the same
`0x10000` twice — once in `map_mmio(bar_addr, 0x10000)` and once as `let
bar_size = 0x10000u64;` — and hands the second copy down. If they ever
disagreed in the loose direction, the closure would admit an offset that
`Mmio::check`'s `assert!` then panics on, in the one file whose header promises
it cannot.

**Proposed shape.** `pub fn size(&self) -> u64 { self.size }` on `Mmio`, and
`legacy::take_ownership(bar: &Mmio, hccparams1: u32)` deriving its own bound.
**What it deletes:** one parameter, one argument, the duplicated literal in
`init_one`, and the possibility of the two disagreeing. `mod.rs:798-806`'s
`checked_sub` pair stays — it is doing real work (usb-storage.md F7's fix) and
needs a length, not a bound.

---

## Part 2 — examined and deliberately not flagged

- **`Bot`, `Scsi`, `Host<'_>`, `Function`, `Walk`, `Resolution`, `WalkError`,
  `BootPartition`/`BootVolume`, `StorageGeometry`, `BlockError`.** Already
  right, and `legacy::find -> Result<Option<u64>, WalkError>` is the model for
  the whole scope: three outcomes, all three reachable, and a selftest that
  requires each by name.
- **`Printable`.** A device-supplied ASCII field that cannot choose what the log
  looks like. Correct, and the only one of its kind in the tree.
- **`Endpoint` as one type for both interrupt and bulk, with a field each kind
  ignores.** The doc argues it and the argument is right: two types means two
  copies of the constructor, and the constructor is the invariant. F2 is about
  the visibility, not the shape.
- **`transfer_blocks`' `assert_eq!(host.len(), count * HOST_BLOCK)`.** A kernel
  contract, documented as one, and every caller constructs the slice from
  `count` by the same arithmetic. Counted as evidence in F8; not a separate
  finding.
- **`bot`'s `assert!(cdb_len <= cdb.len() && cdb_len <= 16)` and
  `assert!(data_len <= MSC_DATA_LEN)`.** The first is over this file's own
  literals. The second is usb-storage.md F6 (a bound naming the wrong buffer)
  and is filed there.
- **`usb_gate.rs`'s `HOST_BLOCKS: [i64; 2] = [1, -1]` and `at()`.** A negative
  index meaning "from the end" is a sentinel, but there is no collision — `-0`
  does not exist — and `usb-gate-teeth.md` verified byte-for-byte that the
  kernel's `at()` and the harness's `block_of()` are the same function, which is
  the property that matters. The shadowing of `at` the function by `at` the byte
  offset (`:165`) and `at` the mismatch position (`:185`) is genuinely confusing
  and is one rename; too small to file.
- **`Partition::lba_count`'s unchecked `last - first + 1` on a `pub struct` with
  `pub` fields.** `storage-stack.md` §Nit 2 filed it with a reproducer. Now
  *reached* by the kernel (`gpt.rs:125`, `:142`) but only on a `Partition` that
  `locate` validated. Same finding, new consumer; not re-filed.
- **`DeviceSectors::lba_count` rounding down to whole 4 KiB blocks**, so the
  backup GPT at the last LBA is unreachable. Filed twice already
  (`storage-stack.md`, `usb-storage.md` Part 5).
- **`READY_BUDGET_NS`'s second clause** and **`MIN_DEVICE_BLOCKS`'s comment.**
  usb-storage.md F11 and F9; F9 is fixed (`9cb9916`), F11's sentence is still
  there and still filed.
- **`esp_log`'s "It costs nothing when nothing is logged".** Filed — CLAUDE.md's
  `specs/issues/` entry carries the correction (userland `println!` is in the same
  ring, so "nothing is logged" is not the idle state).
- **`FileSystem`'s `Result<(), &'static str>` error channel.** Stringly typed,
  and `EspFs` maps `toyos_fat32::Error` into two of them at `:612-617`. It is the
  VFS trait's shape, shared with every other mount, and predates this scope.
- **`EspFs::delete` returning `bool` and logging on the failure path.** The
  `log!`-is-work hazard that F1 of usb-storage.md turned into a write loop
  applies, but `delete` is not on the flush path — `rotate` calls it at most once
  per `MAX_LOG_BYTES`, so it cannot self-sustain.
- **`no_write_cache` as a `bool` latch on `MscDevice`.** A one-way flag whose
  only job is "report this once", with the reasoning written out and correct.
- **`MscDevice` being `Copy` and take-modify-write-back.** Explicit, documented,
  and the borrow it avoids is real.
- **`Drop` guards.** There is no `impl Drop` anywhere in scope. CLAUDE.md's
  caveat does not apply; F1 is the one place a guard was worth considering and it
  is the wrong fix there for a reason that is not the usual one.
- **`unsafe impl Send for XhciController`.** Justified in place, protected by
  `XHCI`'s `Lock`.
- **The `#[must_use]` messages on `BlockDevice`.** Still the best in the tree —
  each names the consequence rather than the rule.

---

## Part 3 — claims checked, including the ones that held

A clean probe is a result, and three of these were the ones most worth breaking.

| # | claim | verdict |
|---|---|---|
| 1 | `mount_boot`: *"A boot sector describing more than the partition holds is already `Error::Truncated`, so this only ever shrinks"* | **Holds.** `boot.rs:185-188`: `if volume_bytes > capacity { return Err(Error::Truncated) }`, before `Geometry` is returned. So `esp.len` and `volume.bytes` can only be narrowed, and `EspDevice`'s headline invariant survives F9's duplication |
| 2 | `Endpoint`: *"`Self::new` is the only way to make one"* | **Does not hold** → F2. Demonstrated with `rustc` |
| 3 | `MscInterface`: *"`bind` has nothing left to check"* | **Does not hold** for the same reason → F2 |
| 4 | `FLUSH_SENSE`: *"only the CSW's verdict is replaced"* | **Does not hold** → F6 |
| 5 | Can `in_dci == out_dci`? | **No.** IN endpoints get odd DCIs, OUT even, by construction. `1 << in_dci \| 1 << out_dci` cannot silently set one bit |
| 6 | Can `transfer_blocks`' `sector_lba as u32` truncate? | **No.** `last_lba <= u32::MAX` ⟹ `(blocks - 1) * spb <= 2^32 - spb`. Exact |
| 7 | Can `EspBacking::read_page` overflow `extent.offset + within`? | **No.** `cluster_offset` is bounded by `first_data_sector * bps + (max_cluster - 2) * bpc`, ~10^14 at the extremes |
| 8 | Does `EspBacking::read_page` walk a page spanning two extents correctly? | **Yes**, in both the extent-capped and `valid`-capped cases |
| 9 | Do `Endpoint`'s `pub(super)` fields inside a `pub struct MscInterface` trip `private_interfaces`? | **No** — `mod msc;` is private, so `MscInterface`'s reachability is already `pub(in xhci)`. My first reproducer's warning was an artifact of making the modules `pub`; the faithful one is silent |
| 10 | Is the `bring_up` failure path reachable in the suite? | **No.** 0 occurrences of `transport broke`, `reset recovery failed`, `never became ready` in a full `usb_storage_gate` boot at this HEAD — so F1 is entirely unexercised, and so is `dev.failed` |
| 11 | Are the two mass-storage pool blocks distinct in a two-disk boot? | **Yes** — `msc_block +0x10000` and `+0x20000` both observed, `MSC_STRIDE = 0x10000` |
| 12 | Does the tree pass with the working tree as read? | **Yes** — `cargo test -- usb_storage`, 3 passed, 20.0 s |

---

## Part 4 — what could not be checked

- **Anything that needs a device to fail.** F1 needs a `bring_up` that times out
  *and* a second disk; F2 needs a second constructor that does not exist; F3
  needs a failing read on the ESP. QEMU stages none of the three, which is the
  same structural limit `specs/device-test-strategy.md` describes and which
  `usb-gate-teeth.md` Part 2 already enumerates for this exact subsystem. All
  three are argued by construction and say so.
- **Whether a real xHC completes a TRB abandoned by a `wait_transfer` timeout.**
  F1's sharpest consequence rests on it. The endpoint is left in the Running
  state with the TRB fetched, which is what the spec's state machine implies and
  what `clear_stall`'s existence assumes, but I have not observed it. The
  *unconditional* half of F1 — the pool block is re-issued while a slot and two
  endpoint contexts still name it — does not depend on that question.
- **The `usb-flush-fails` / `usb-flush-unimplemented` gates themselves.** F6 is
  read off the actuator; I did not run either feature.

---

## Is this code systematically different from the older drivers?

**Yes, and the difference is a mechanism rather than a standard — which is why
the tree is converging and also why the residual is so predictable.**

Every type in this scope was added *after* something bit, at the exact site
where it bit. `Bot` and `Scsi::Refused` exist because reading an optional
command's refusal as a device failure turned the idle loop into a write loop.
`Endpoint` and `Walk` exist because a zero endpoint address was doing double
duty as "not filled in yet". `Cluster` exists because a crafted directory entry
drove a write 256 GiB outside a volume. `BlockError` and the three `#[must_use]`
messages exist because the page cache served one block's bytes under another
block's number. `settles()` exists because two register spins hung a boot with
no output. Not one of them was prophylactic. Against `kernel-drivers.md` §2's
count — five of ten recorded hardware defects in classes a type closes, two of
those five fixed by adding types — this scope is running at a much better rate,
and it is the *same* rate: types arrive where defects arrive.

That is genuinely different from `virtio.rs`, `acpi.rs` and the three copies of
the MSI-X setup, which have not been bitten and still carry the shapes. It is
better than the tree's average by a wide margin: I could not break the
device-value handling by reading it, which matches usb-storage.md's finding, and
`legacy::find` is the best-shaped function in the driver directory.

But the mechanism has a signature, and every finding above has it. The fix stops
at the layer of the defect and the layer outward keeps the old shape.
`MscInterface` got a constructor-enforced invariant and `MscDevice` — three
zeroed geometry fields completed by `bring_up`, and the block it lives in — did
not. `Endpoint` got a private constructor and not the private *field* that would
make it one. `BlockDevice` got an error channel and `FileBacking`, one call
outward, still returns `()` — with the new implementation being the only one of
three that does not even log. `write_ctx32` is the one function in the driver
that takes a raw pointer and an unbounded index, at 23 sites, and it is what
`Endpoint`'s whole invariant is *for*.

So: converging, not accumulating — but converging one layer per defect. The
useful prediction is that the next hole is not somewhere new; it is one call
outward from `9dfd044`, and F1, F2 and F3 are where that is.

---

## Closed

Worked in the session after this audit was written. Each was verified red before
and green after, with the fix backed out and the actuator left in.

| # | commit | gate | evidence |
|---|---|---|---|
| F2 | `be70cc3` | the compiler | the reproducer's `Endpoint { dci: 200, .. }` in `msc.rs` builds with zero warnings against the tree with the field `pub(super)`, and is `E0451` with it private. `write_ctx32`'s parameter is `ctx_index`; it gets no bound and now states why it needs none |
| F3 | `64b89b8` | `esp_backing_read_error` | with the fix backed out: **3247 NUL bytes at offset 4096** of a 14,750-byte `kernel.log`, read off the image on the host |
| F1 | `05ae01e` | `usb_refused_disk_first` | with the fix backed out: `disk 0 ready on slot 2 … msc_block +0x10000`, the block the disk refused on slot 1 had just configured its endpoints into |
| F7 | `05ae01e` | — | `bind`'s `-> bool` deleted with F1, same function |

Three notes for whoever takes the rest.

**F3's reach was larger than the finding.** The log line was the trivial half, as
predicted. The other half was that `FileBacking::read_page` returned `()`, so
`file_cache::write_page` could not tell a hole from data and merged a partial
write into it. Making the trait fallible moved five more call sites:
`file_cache::read_page` (which must not *cache* a failed fetch — the same
corruption through the other door), `esp_log::append`, `fd::try_write`,
`process::handle_page_fault`, `elf::read_backing_into` and
`loader::read_file_range`. The residual — `fd::try_write` has no honest errno —
is filed under §1.

**The prediction held, and one layer further than stated.** "The next hole is
one call outward from `9dfd044`" was right for all three. F3's own fix then had
a hole one call outward *of it*: `write_page` refusing is useless if
`read_page` caches the zeros instead, and that call site is in the same file
and was not in the finding.

**An injection that replaces only the verdict can make its own gate vacuous.**
The first version of `esp-backing-read-fails` overrode the return of
`EspBacking::read_page` but left the buffer holding the bytes the device had
actually delivered — so the corruption never materialised and the host-side
assertion passed with the defect present. It now replaces verdict *and* buffer,
which is the state a real failed read leaves. F6 below is the same shape and is
still open; this is what it looks like when it bites.

## Not taken

- **F4** (`gpt::probe`'s `lba_bytes`), **F5** (`with_storage -> Option` that is
  never `None`, three dead `.unwrap_or(false)`), **F6** (`FLUSH_SENSE`
  overriding a broken transport), **F8** (`count` + `buf.len()` across
  `BlockDevice`), **F9** (`EspDevice.len` / `EspVolume.bytes`), **F10** (`Mmio`
  will not report its size). None is a live defect; F6 is the one with teeth,
  because it is an instrument that can go green on a boot where nothing
  happened, and the session that closed F1–F3 hit exactly that failure mode in a
  new actuator. Take F6 next.
