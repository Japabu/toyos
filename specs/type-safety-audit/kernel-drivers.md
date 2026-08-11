# Type-safety audit: `kernel/src/drivers/` and `kernel/src/arch/`

Read-only audit, 2026-08-01, against the **working tree** at `3e2c975`. 13,073
lines across 33 files (`find … | xargs wc -l`). Nothing was built or run; every
number below comes from a `grep`/`wc` recorded beside it. Numbers describing code
that does not exist yet are marked **(projected)** and were derived by writing
the replacement out, not estimated.

> **Three cited files are not in `main`** — `xhci/msc.rs`, `usb_storage.rs`,
> `usb_gate.rs` are another agent's uncommitted work. **§7 says exactly which
> findings that touches and which are unaffected.** Read it before acting on F3.

## The test applied

CLAUDE.md's lens: *"C-isms tolerated only when the Rust alternative adds no
safety or value. Prefer compile-time safety: unrepresentable > checked at
runtime > covered by tests."*

Each finding had to pass one of two bars:

1. **Name a bug the current shape permits**, or
2. **Be a clear improvement in how the code reads**, judged by writing the same
   call site both ways. A change that deletes code or collapses special cases
   passes on that basis alone, and the deleted lines and constants are counted.

**The size of a change is never an argument against it.** Blast radius is
reported as fact — what it touches, what could break — because sequencing needs
it, never as a reason to soften a recommendation. Findings that were only "this
could be a newtype", with no bug and no readability gain, were dropped; §6 lists
them, because a list of only hits is a monument rather than a record
(`specs/metal-track-history.md` rule 7).

`specs/issues/` was read in full first. Where a finding extends a filed
class it says so and points at the entry; nothing here re-files what is there.

---

## 1. Summary

**This code is mostly not C-shaped.** `ioapic.rs` (`Gsi`, `Trigger`, `Polarity`,
`IsaLine`, `RouteError`), `xhci/msc.rs` (`Bot`, `Scsi`, `Host<'_>`, every device
number refused by name), `virtio.rs`'s `DescSlot` proof token, `serial.rs`'s
`BackendGuard`, `percpu.rs`'s `const _: () = assert!(offset_of!(…))` block, and
`mm/mmio.rs`'s bounds-checked `Mmio` are all better than what a kernel of this
age usually has. Three of them were written *after* the class they encode had
already bitten.

The findings are where those types stop.

| # | Finding | Bug permitted | Code delta |
|---|---|---|---|
| F1 | `Virtqueue::poll_used` hands out a device-chosen index and length with no parse step; three consumers use them raw, one as a pointer offset | **Out-of-bounds read** in `unsafe`; two panic paths; silent descriptor aliasing | −2 hand-written checks, +6 lines at one primitive |
| F2 | MSI-X is programmed three times from three copies that have already diverged | Divergent absent-capability policy; **nobody reads the table-size field before writing entry 0** | **Closed** (#58) — 86 → 71 kernel lines plus `toyos-pci/`; the table-size bound turned out to be unfirable and two others real |
| F3 | `BlockDevice::read_blocks` returns `()`; NVMe discards all 6 completion statuses | Page cache serves one block's bytes under another block's number | +1 enum, `?` at 7 call sites |
| F4 | Zero `#[must_use]` in the scope; 20 counted sites discard a status they already hold | `442f3e8`'s exact class: "the write that arms the pin was the one nobody checked" | +1 attribute, +14 `let _ =` |
| F5 | ACPI tables parsed with no length check and no checksum; two subtractions underflow | Unbounded walk over firmware-chosen memory; boot dies on a legal table set | −4 `unsafe { &*ptr }`, +1 `Table` type |
| F6 | Drivers destructure `KernelSlice` into `_phys: u64` / `_ptr: *mut u8` pairs and carry the halves | Two halves of one region kept in sync by naming convention | **27 counted fields → ~14 (projected)**; 37 counted destructuring lines → ~19 |
| F7 | Interrupt vector numbers declared twice — `Vector` enum and again in three drivers | Device delivers to a vector with no IDT gate; `#GP`, no build error | −3 constants |
| F8 | `KernelSlice::ptr_at` bounds-checks zero bytes and is the escape hatch every driver uses | The doc says "bounds-checked"; the pointer is, every use of it is not | −4 hand-written `assert!`s |
| F9 | `wrmsr` is a safe `fn` while `write_cr3` is `unsafe`; `invpcid` takes three `u64`s | `IA32_LSTAR`/`IA32_GS_BASE` writable from safe code | +1 enum, +1 `unsafe` |
| F10 | i8042 deadlines and durations are bare `u64` in two different units | `wait_writable(500)` compiles and means "expired at boot" | −1 function (`stage`) |
| F11 | 8 MMIO register polls unbounded, in files whose other waits are not | Boot hangs with no output on the one machine with no serial | 8 counted polls → 1 helper, ≈−22 lines |
| F12 | `static mut DMA_HOLDER` in `virtio_console.rs` | Lint; inconsistent with the `ConsoleCell` two lines below | −2 lines |

**Verdict on typed MMIO: no to a general register layer, yes to a typed MSI-X
table.** §3 writes both out and counts. The general layer loses on its own
merits — written both ways, the call site is the same length and the register
definitions are pure addition. The MSI-X table wins on the same test: it deletes
roughly 50 lines and unifies three copies that have already drifted.

---

## 2. The correlation: what the recorded defects say about types

`specs/metal-track-history.md`'s "What QEMU structurally could not find" table is
the best available sample of hardware-shaped defects in this tree — every one
found by reading code against a spec, none by a test. Scoring each against
"would a type have made this unrepresentable?":

| Recorded defect | Type would have closed it? |
|---|---|
| xHCI scratchpad array not page-aligned, one buffer written for N (`5bb673c`) | **No.** Placement arithmetic against a spec rule no type knows. |
| `OP_PAGESIZE` read only inside a `log!` while `PAGE` stayed a hardcoded 4 KiB (`71940c1`) | **Partly.** Not a register type — a `#[must_use]` on the read, or deriving every placement from a `PageSize` newtype rather than from a constant. F4 plus the derived-unit half of F10. |
| `serial::init`'s loopback `assert!` killed the kernel (`2e52e8e`) | **Weakly.** `-> Result` instead of `assert!` makes "what if it is absent" a question the caller must answer. |
| Framebuffer memory-type gate classifies only the scanout's first byte (`938c7ac`, filed, unfixed) | **Yes, in shape.** `maps.iter().find(\|e\| phys >= e.start && phys < e.end)` is a point query where the value is a *range*. A `PhysRange` with an `overlaps` predicate makes the point query unwritable. |
| I/O APIC reported success on a machine with nothing wired up, three ways (`09f40be`) | **Yes — and it was fixed by types.** The result is `ioapic.rs`: `Gsi`, `IsaLine` bundling trigger+polarity so a caller "cannot take the GSI and forget the electrical properties", `RouteError` as a `Result`, and a read-back inside `route`. |
| "Every i8042 write is read back" false for the one write that arms the pin (`442f3e8`) | **Yes.** Exactly F4. The fix added a read-back at that one site; the other 14 statement-position `command(…)` calls in the same file still discard their `bool`. |
| Mouse framer's resync claim (`01f46c5`) | **No.** Protocol reasoning about a byte stream. |
| `wait_command` dropped foreign TRBs; `wait_transfer` returned the first Transfer Event's code (`40aff72`) | **Yes, in shape.** "An event is not yours because it is the right shape" is the identity-in-the-type problem. F1 is the same statement about descriptor ids. |
| `fn virtio(self) -> bool { self != Profile::Metal }` fail-open for every future variant (`4752f83`) | **Yes.** Exhaustive `match` over the enum instead of `!=`. Test-side, same lens. |
| NVMe namespace reporting 8 KiB sectors said `#DE` before it said why (`fa1e9d4`) | **Yes.** `lba_ds` is a shift exponent, `sector_size` is bytes, `sectors_per_block` is a ratio, all `u32`. F1's sibling, F3's motivation. |

**Five of ten are in classes a type closes; two of those five were in fact fixed
by adding types.** That is the most useful thing this audit can say: the modules
that have been bitten are the modules that now carry the types, and the modules
that have not been bitten yet — `virtio.rs` and its four consumers, `acpi.rs`,
`block.rs`, and the three copies of the MSI-X setup — are where the same shapes
are still open.

It also bounds the claim honestly. Half the recorded hardware defects were
arithmetic against a specification, and no type system in reach would have
touched them.

---

## 3. Typed MMIO, decided by writing it both ways

### What exists

`kernel/src/mm/mmio.rs` — a `Copy` handle with `base`/`size`, a `subregion` that
asserts containment, and six accessors each asserting `offset + len <= size`.
Counted: **132** driver call sites (`grep -rnE "\.(read|write)_u(8|16|32|64)\("
kernel/src/drivers/`), **25** `const NAME: u64 = 0x…` register offsets, **81**
named bit constants, **25** shift-and-mask field extractions (19 in `drivers/`,
6 in `arch/`), **20** inline `1 << N` bit literals. `bitflags` is not a
dependency (0 hits in either `Cargo.toml`).

So the out-of-window class is already closed, and registers and bits are already
named. The question is only whether a typed register reads better.

### The general layer, written both ways

xHCI's PORTSC is the best case in the scope — a register with a W1C rule, read
at four sites, with a field extraction. Current, `xhci/device.rs:191-209`:

```rust
let portsc_off = OP_PORT_BASE + port_idx as u64 * PORT_REG_SIZE;
let portsc = op_base.read_u32(portsc_off);
op_base.write_u32(portsc_off, (portsc & !PORTSC_RW1C) | PORTSC_PR);
loop {
    let ps = op_base.read_u32(portsc_off);
    if ps & PORTSC_PRC != 0 { break; }
    core::hint::spin_loop();
}
let portsc = op_base.read_u32(portsc_off);
op_base.write_u32(portsc_off, (portsc & !PORTSC_RW1C) | PORTSC_PRC);
let portsc = op_base.read_u32(portsc_off);
if portsc & PORTSC_PED == 0 { … }
let speed = ((portsc >> 10) & 0xF) as u8;
```

Typed:

```rust
let port = op_base.port(port_idx);
port.set(Portsc::RESET);                       // W1C bits preserved by the type
port.wait(|p| p.reset_change(), deadline)?;
port.ack(Portsc::RESET_CHANGE);
let p = port.read();
if !p.enabled() { … }
let speed = p.speed();
```

The typed version reads better — the `& !PORTSC_RW1C` idiom is a correctness
rule of the register that the type enforces instead of the author remembering,
and it is the kind of thing that goes wrong silently (forget it and setting one
bit clears every status bit as a side effect). **That is a real gain and it is
worth taking for PORTSC specifically.**

But it does not generalise, and the counting is why. Across the scope there are
**25** field extractions against **25** register-offset constants — one
extraction per register on average. A `register!` macro emitting a type with
named accessors costs at minimum one line per field plus a header, so 25 fields
across ~15 registers is on the order of 40 lines added. What it replaces is 25
one-line expressions that become 25 one-line method calls. **Written both ways,
the call sites are the same length and the definitions are pure addition.** The
remaining 107 of the 132 accessor sites are whole-register reads and writes with
no fields at all — `common.write_u32(COMMON_DEVICE_STATUS, status)`,
`unit.write(REG_REDTBL + 2 * n, low)` — and a typed register changes none of
them.

And the record agrees: **not one of the ~70 confirmed defects was a wrong offset
or a wrong width**, which is all the layer adds over the bounds check already in
place. `71940c1` read the right register at the right width and did not use the
value. `442f3e8` was a missing read-back — a property of the write *protocol*.
`09f40be` was believing a window that answers `0xFFFFFFFF` — a property of the
value's *plausibility*.

**Verdict: no.** Not because it is a lot of work, but because writing it out
shows it adds code without changing what the call sites look like, and it aims
at a class with zero recorded instances. The one register where the type does
read better is PORTSC, and that belongs with the xHCI work as a local change,
not as a layer.

### The MSI-X table, written both ways — this one wins

This is the part of "typed MMIO" that does delete, and it was under-weighted
until the snippets were written out. It is F2 below.

### Typestate

**Do not build:** `VirtioDevice`'s reset → ACKNOWLEDGE → DRIVER → features →
FEATURES_OK → `setup_queue` → `enable_queue` → `activate`. Written both ways,
the typestate version is longer: eight marker types and eight `impl` blocks to
express a sequence that is `VirtioDevice::init` (`virtio.rs:398-445`) plus four
statements per driver, run once at boot on one CPU. The thing worth a token —
"this descriptor is free to submit into" — **already has one**: `DescSlot`,
non-`Copy`, minted by `poll_used` and consumed by `submit`. Same argument for
`NvmeController`'s reset → AQA/ASQ/ACQ → enable → identify (`nvme.rs:392-441`).

**Build it with hotplug, not before:** xHCI's serial-enumeration invariant. One
EP0 ring, one input context and one descriptor buffer serve every device, and
the only thing making that sound is that `init_device` runs once per port from
`init` on one CPU. `device.rs:425` and `mod.rs:reset_ep0_ring` write the
invariant in prose; `specs/issues/hardware/` records that hotplug is precisely what
breaks it and that the fix is an enumeration lock. **A token —
`EnumerationLock` minted once by `scan_ports` and required by `init_device` —
makes the prose a signature.** Today `scan_ports` has one caller so the token
proves nothing; as part of hotplug it is the difference between an invariant and
a comment. Recorded so whoever picks up hotplug sees it.

---

## 4. Findings

### F1 — `Virtqueue::poll_used` returns a device-chosen index and length with no parse step

**Location.** `kernel/src/drivers/virtio.rs:357-369`; consumers at
`virtio_console.rs:132-148`, `virtio_net.rs:88-99`, `virtio_gpu.rs` and
`virtio_sound.rs` via `submit_and_wait`.

**Current shape.** Both values in the used-ring element are written by the
device:

```rust
pub fn poll_used(&mut self) -> Option<(DescSlot, u32)> {
    …
    let id  = unsafe { read_volatile(self.used_ring_id_ptr(slot)) };   // device-written u32
    let len = unsafe { read_volatile(self.used_ring_len_ptr(slot)) };  // device-written u32
    self.last_used_idx = self.last_used_idx.wrapping_add(1);
    Some((DescSlot(id as u16), len))
}
```

`DescSlot` is the codebase's own proof token — *"non-Copy, non-Clone: must be
obtained from `poll_used()` or `initial_slots()`"* (`virtio.rs:161-164`). It
proves the descriptor is *free*. It says nothing about the number being in
range, and `id()` is public.

**Bugs permitted, in severity order.**

1. **An out-of-bounds read, in `unsafe`, from a device-chosen length.**
   `virtio_console.rs:136-147`:

   ```rust
   let (slot, len) = c.rx.poll_used()?;
   let buf_idx = c.desc_to_rx[slot.id() as usize] as usize;
   c.rx_pending = Some(RxPending { buf_idx, slot, len, pos: 0 });
   …
   let byte = unsafe { *c.rx_ptrs[p.buf_idx].add(p.pos as usize) };
   p.pos += 1;
   if p.pos >= p.len { … }
   ```

   `len` is never compared against `RX_BUF_SIZE` (256, `virtio_console.rs:29`).
   The buffers are 256 bytes apart inside one page (`:186-191`), so a device
   reporting a larger length walks the next RX buffer, then `OFF_RXVQ`'s
   virtqueue rings, then past the DMA pool — all inside the direct map, so it
   faults nowhere and feeds kernel memory to the console as input bytes.

2. **A kernel panic from an index.** `desc_to_rx` is `[u8; 16]`
   (`virtio_console.rs:56`) and `desc_to_buf` is `[u16; 256]`
   (`virtio_net.rs:57`); `slot.id()` ranges over `u16`.

3. **Silent descriptor aliasing.** `virtio_gpu` and `virtio_console` put the
   returned slot straight back into `control_slot`/`tx_slot`, and `submit`
   indexes `(first_desc + i as u16) % size` (`virtio.rs:316`) — so a bogus id
   wraps and overwrites another submission's descriptors rather than failing.

**The asymmetry is the finding.** `virtio_sound.rs` checks the same value twice
(`:229` `assert!((desc_id as usize) < TXQ_SIZE)`, `:441` `assert!(id <
EVENT_BUFS)`); `xhci/msc.rs`'s module doc sets the standard explicitly —
*"everything that arrives here came off a wire, so nothing in this file may panic
on it"*. Four consumers of one primitive, three standards.

**Both ways.** Today, at the primitive:

```rust
Some((DescSlot(id as u16), len))
```

and then each consumer either writes its own `assert!` (virtio-sound, twice) or
does not (virtio-net, virtio-console, virtio-gpu). Proposed:

```rust
// virtio.rs — the one place a device number enters this kernel.
let id = unsafe { read_volatile(self.used_ring_id_ptr(slot)) };
let len = unsafe { read_volatile(self.used_ring_len_ptr(slot)) };
self.last_used_idx = self.last_used_idx.wrapping_add(1);
if id >= self.size as u32 || len > self.chain_len[id as usize] {
    self.protocol_errors += 1;   // the slot is forfeit; never a read outside the pool
    return None;
}
Some((DescSlot(id as u16), len))
```

**Call-site change.** `virtio_sound.rs:229` and `:441` lose their `assert!`s;
`virtio_console.rs:141`'s `unsafe` read becomes bounded by construction;
`virtio_net.rs:90`'s index becomes provably in range. **Net: two hand-written
checks deleted, six lines added at the primitive, and the two consumers that
never checked stop needing to know that they should.**

**Blast radius.** One primitive, four consumers, no ABI. `chain_len` needs
`submit` to record each chain's byte total — one array of `u32` sized by
`self.size`.

**Standing.** Extends `40aff72`'s class ("an event is not yours because it is
the right shape") from the xHCI event ring to the virtio used ring. Not filed.

---

### F2 — MSI-X is programmed three times, from three copies that have already diverged

**Location.** `xhci/mod.rs:618-647`, `virtio_net.rs:129-171`,
`virtio_sound.rs:469-509`.

**Counted.** 23 + 33 + 32 = **88** non-blank, non-comment lines. **15** lines are
byte-identical between xHCI and virtio-net after whitespace normalisation;
**24** between virtio-net and virtio-sound (`comm -12` on sorted, stripped
bodies).

**Bugs permitted.**

1. **The copies have already drifted, in the direction that matters.** A machine
   with no MSI-X capability: xHCI logs and degrades to polled mode
   (`mod.rs:623`); virtio-net and virtio-sound `panic!` (`virtio_net.rs:132`,
   `virtio_sound.rs:472`). Two more `panic!`s each on the vector-assignment
   read-back. That is three different answers to "this machine's device is not
   shaped the way we assumed" — the class M1 closed for xHCI's zero-HID panic
   and it is filed twice more (NVMe absence; soundd's `num_buffers > 5`,
   `specs/issues/audio/soundd-panics-on-a-shallow-pipeline.md`).
2. **Nobody reads the table-size field.** `msg_ctrl & 0x7FF` is *table size − 1*;
   `grep -rn "0x7FF\|table_size" kernel/src/drivers/` returns **0**. All three
   copies write entry 0 of a table whose length they never asked about, and none
   validates `table_bir` against the number of BARs the device has before
   `read_bar_64(table_bir)`. On QEMU every device has a table; a Lenovo
   controller reporting a zero-length table is written into anyway.
3. Each copy independently hardcodes `0xFEE0_0000` and the entry-0 offsets
   `0x00/0x04/0x08/0x0C`, three times.

**Both ways.** Today, in each of three drivers:

```rust
let cap = pci_dev.capabilities().find(|c| c.id() == PCI_CAP_MSIX);
let cap = match cap { Some(c) => c, None => { log!("…"); return; } };
let table_info   = cap.read_u32(4);
let table_bir    = (table_info & 0x7) as u8;
let table_offset = (table_info & !0x7) as u64;
let table_bar    = pci_dev.read_bar_64(table_bir);
let table_addr   = table_bar + table_offset;
let table = crate::mm::paging::kernel().lock().as_mut().unwrap().map_mmio(table_addr, 0x1000);
table.write_u32(0x00, 0xFEE0_0000);
table.write_u32(0x04, 0);
table.write_u32(0x08, XHCI_VECTOR as u32);
table.write_u32(0x0C, 0);
let msg_ctrl = cap.read_u16(2);
cap.write_u16(2, (msg_ctrl | (1 << 15)) & !(1 << 14));
```

Proposed, in `pci.rs`, once — and note it can now report the two things no copy
checks:

```rust
/// A device's MSI-X table, claimed once. `entries` is what the device says it
/// has, so routing entry N past it is a `Result` rather than a write into a
/// window that decodes to nothing.
pub struct MsixTable { table: Mmio, cap: u64, entries: u16 }

impl MsixTable {
    pub fn claim(dev: &PciDevice) -> Option<Self> { … }
    pub fn route(&self, entry: u16, vector: u8, dest_apic_id: u32) -> Result<(), MsixError> { … }
    pub fn enable(&self, dev: &PciDevice) { … }
}
```

and at each of the three call sites:

```rust
let Some(msix) = MsixTable::claim(&pci_dev) else {
    log!("xHCI: no MSI-X capability, using polled mode");
    return;
};
msix.route(0, XHCI_VECTOR, apic::id())?;
msix.enable(&pci_dev);
```

**Code delta.** 88 counted lines today. The shared form is one ~20-line type
plus what is genuinely per-device — three lines for xHCI, and the two virtio
drivers keep their ~8 lines of `COMMON_MSIX_CONFIG` / `COMMON_QUEUE_MSIX`
programming, which is virtio's protocol and not MSI-X's. **≈38 lines, so ≈50 go
(projected).** Three copies of `0xFEE0_0000` become one; three copies of the
entry-0 offsets become one; three different absent-capability policies become
one that each driver states in its own words at its own call site, which is where
the difference belongs.

It also gives `route` a natural home for the destination-id check that
`ioapic::route` already does (`ioapic.rs:289-291`) and MSI-X does not — the
identical `0xFF`-is-broadcast hazard, on the other delivery path.

**Blast radius.** Three drivers, one new type in `pci.rs`. The two virtio drivers
are not under active edit; `xhci/` is.

**Standing. Closed** (task #58), in a different shape than proposed above —
recorded because the difference is the projection's mistake and not the
implementation's.

- **No `MsixTable` type.** Every call site arms one entry, so `route(entry, …)`
  would have been a parameter nobody varies, and the table-size bound it existed
  to enforce cannot fire: Table Size is encoded *one less than it is*, so a
  function that has the capability at all has at least one entry and entry 0
  needs no check. The shared form is `PciDevice::enable_msix(vector)`, which was
  already there, plus `MSIX_ENTRY` naming the entry once.
- **The decode moved out of the kernel**, to `toyos-pci/` — with `enable_msi`'s
  register layout for company, since that one *does* shift by four bytes on a
  64-bit-address function and no test could reach the arithmetic. 15 host tests,
  each with its negative control run.
- **Two of the numbers being believed are real bugs, and neither is the table
  size.** A reserved BIR (6 or 7) sent `read_bar_64` into the CardBus CIS
  pointer at config offset 0x28 and mapped whatever that decoded to; a BAR
  firmware left unassigned put the table at physical 0. Both refuse by name now.
  The third — an I/O BAR, which `read_bar_64` cannot see at all — is filed in
  `specs/issues/isolation/` rather than fixed, because the fix changes that function's
  signature at four call sites.
- **Counted, and it is not −50.** Kernel-side MSI-X setup went 86 → 71 lines
  (21+33+32 → 28 in `pci.rs`, 13 in `virtio.rs`, 15 in each virtio driver), and
  `toyos-pci` is 119 more with 100 of tests. The projection left out
  `enable_msi`, and left out that replacing two `panic!`s with refusals costs
  lines a panic did not.

The destination-id check is still not built, and now has nowhere to go: every
message this kernel programs goes to APIC id 0 by one constant, so there is no
destination for a call site to get wrong.

---

### F3 — `BlockDevice::read_blocks` returns `()`, so a failed read is indistinguishable from a successful one

**Location.** `kernel/src/block.rs:14,18,21`. Implementations at
`drivers/nvme.rs:333` and `drivers/usb_storage.rs:58`. Seven callers
(`grep -rn "\.read_blocks(" kernel/src/`).

**Bug permitted, live in both implementations.**

- **NVMe throws the completion status away at every site.** `submit_and_wait`
  returns the 15-bit status field (`nvme.rs:121-124`) and **all six callers
  discard it** — `:167`, `:180`, `:192`, `:205`, `:262`, `:297`. A failed READ
  returns whatever the DMA data buffer held and `copy_nonoverlapping`s it into
  the caller's slice (`:264`).
- `usb_storage.rs` *has* the answer — `xhci::storage_read` returns `bool` — and
  can only log it (`:67-71`), which its own doc comment says: *"the trait is what
  has to change"*.

**Where the harm lands.** `page_cache.rs:228-237`:

```rust
let slot = self.alloc_slot(dev, block);
let page = self.slot_data_mut(slot);
dev.read_blocks(block, 1, page);
self.slot_data(slot)
```

`alloc_slot` evicts and reuses a slot that still holds the *previous* block's
bytes. A failed read leaves them there, and the cache serves one block's
contents under another block's number, indefinitely. Same consequence as the
filed `FileBacking`-after-unlink leak (`specs/issues/isolation/`, *"reads blocks the
allocator has since given to another file"*) by an entirely different route, and
not filed.

`gpt.rs:221` reads the partition table the same way. `usb_gate.rs:90` is the one
caller where the missing channel is harmless, and the reason is worth stating:
`head` is `vec![0u8; BLOCK]`, so a failed read leaves zeros, the `MAGIC`
comparison fails, and the gate refuses to write. **Fail-safe by accident of
initialisation, not by construction** — which is exactly the property CLAUDE.md's
"the kernel never formats a disk it was not given" should not rest on.

**Both ways.** Today, `nvme.rs:262`:

```rust
self.io.submit_and_wait(&self.bar, cmd);
unsafe { copy_nonoverlapping(dma.ptr_at(OFF_DATA) as *const u8, buf.as_mut_ptr(), total_bytes); }
```

The status is right there and dropping it takes no syntax. Proposed:

```rust
let status = self.io.submit_and_wait(&self.bar, cmd);
if status != 0 {
    return Err(BlockError::DeviceFailed);
}
unsafe { copy_nonoverlapping(dma.ptr_at(OFF_DATA) as *const u8, buf.as_mut_ptr(), total_bytes); }
Ok(())
```

with

```rust
/// Why a block operation did not happen. Not an errno: the layer above needs
/// to distinguish "this device is gone" from "this request was wrong".
pub enum BlockError { DeviceFailed, OutOfRange }

pub trait BlockDevice: Send {
    fn read_blocks(&mut self, lba: u64, count: u32, buf: &mut [u8]) -> Result<(), BlockError>;
    fn write_blocks(&mut self, lba: u64, count: u32, buf: &[u8]) -> Result<(), BlockError>;
    fn flush(&mut self) -> Result<(), BlockError>;
}
```

**Call-site change**, `page_cache.rs:235`:

```rust
- dev.read_blocks(block, 1, page);
- self.slot_data(slot)
+ dev.read_blocks(block, 1, page)?;      // `read` becomes -> Result<&[u8], BlockError>
+ Ok(self.slot_data(slot))
```

`usb_storage.rs:67-83` loses its three `if !… { log!(…) }` blocks and becomes
three one-line delegations, and its `healthy()` method (`:44-56`) — which exists
only because the trait has no error channel — goes with them. **Net at the
implementations: −12 lines; net at the callers: `?` added.**

**Blast radius.** Two implementations, seven direct callers, and the `?`
propagates into `page_cache`, `gpt`, `vfs` and the bcachefs adapter, none of
which can fail a read today. Every one of those is a place that currently
believes a buffer it should not.

**Standing.** Same class as `fa1e9d4`. Unfiled — `usb_storage.rs:44-56` says
"filed rather than changed here" but no entry exists in `specs/issues/`.

---

### F4 — Zero `#[must_use]` in the scope, while 20 counted sites discard a status they already hold

**Location.** Everywhere. `grep -rn "must_use" kernel/src/` returns **7**, and
**none is under `drivers/` or `arch/`** — they are `hw.rs:34`,
`shared_memory.rs:135`, `scheduler.rs:168`, `process.rs:152/:277/:657`,
`sched/driver.rs:362`.

**Counted instances.**

- NVMe: **6 of 6** `submit_and_wait(…) -> u16` calls discard the status (F3).
- i8042: 16 statement-position calls to the four `-> bool` primitives
  (`command`, `write_data`, `write_config`, `flush`); **2 use the result**
  (inside `read_config` at `:465`, `write_config` at `:469`), **14 discard it** —
  `:551`, `:553`, `:600`, `:601`, `:632`, `:641`, `:643`, `:652`, `:654`,
  `:656`, `:660`, `:714`, `:737`, `:741`.

**Bug permitted.** The one the tree already paid for. `442f3e8`, verbatim from
`specs/metal-track-history.md`: *"'Every i8042 write is read back' was false for
the one write that arms the pin. A controller that drops `CFG_PORT1_IRQ` still
fills the output buffer and never asserts — invisible from every angle."* The fix
added a read-back at that one site (`i8042/mod.rs:791-807`). Fourteen sibling
writes in the same function are still unchecked, and the same reasoning applies
to each: `command(CMD_SELF_TEST, …)` at `:632` that never reached the controller
is followed by a `read_data` that reads something stale, and the only thing
catching it is the `Some(0x55)` comparison two lines down — a *value* check
standing in for a *delivery* check.

Most of the fourteen are defensible. The point is that **nothing distinguishes
the defensible ones from the one that cost `442f3e8`**, because discarding is the
default and silent.

**Both ways.** Today:

```rust
command(CMD_DISABLE_PORT1, budget);
command(CMD_DISABLE_AUX, budget);
```

Proposed — the attribute plus, at each site the author still wants to discard,
the reason:

```rust
#[must_use = "a controller that dropped this write still fills the output \
              buffer and never asserts; the caller must decide"]
fn command(cmd: u8, deadline: u64) -> bool { … }
```

```rust
// Firmware may have left scanning on; either way the config read-back below
// is what establishes the controller answered.
let _ = command(CMD_DISABLE_PORT1, budget);
let _ = command(CMD_DISABLE_AUX, budget);
```

That is longer, and it is better: the fourteen silent decisions become fourteen
stated ones, and the fifteenth — the one someone adds next year — cannot be made
by accident.

For NVMe the honest form is not `#[must_use] -> u16` but F3's `Result`; there is
no correct way to discard it.

**Blast radius.** One attribute per primitive; `let _ =` at 14 sites in a file
under active edit.

**Standing.** Directly extends `442f3e8`. Not filed as a class.

---

### F5 — ACPI tables are firmware input parsed with no length check and no checksum

**Location.** `kernel/src/drivers/acpi.rs`.

**Current shape**, `find_table:264-281`:

```rust
let header = unsafe { &*xsdt.as_ptr::<SdtHeader>() };
let length = header.length as usize;
let entry_count = (length - size_of::<SdtHeader>()) / 8;
let entries_base = unsafe { xsdt.as_ptr::<u8>().add(size_of::<SdtHeader>()) } as *const u64;
for i in 0..entry_count {
    let table_phys = DirectMap::from_phys(unsafe { read_unaligned(entries_base.add(i)) });
    let table_header = unsafe { &*table_phys.as_ptr::<SdtHeader>() };
    if &table_header.signature == signature { return Some(table_phys); }
}
```

`parse_madt:306` has the identical shape: `let entries_len = length -
size_of::<Madt>();`.

**Bugs permitted.**

1. **Underflow.** `header.length` is firmware's `u32`. Any value below 36
   (`SdtHeader`) makes `entry_count` a value near `usize::MAX / 8` in release,
   and the loop reads u64s out of arbitrary memory and dereferences each as an
   `SdtHeader`. In debug it is a panic before `idt::init`, i.e. a triple fault
   with no report.
2. **No checksum, anywhere.** Every ACPI table carries one; none is verified.
3. **No revision gate on the RSDP.** `get_xsdt:257-260` reads `rsdp.xsdt_address`
   unconditionally; on an ACPI 1.0 RSDP that field does not exist and the read is
   past the structure.
4. **A legal firmware shape kills the boot.** `init_power:203`
   `.expect("ACPI: FADT not found")`, `:215` `assert!(dsdt_addr != 0)`, `:221`
   `.expect("ACPI: \\_S5_ not found in DSDT")`. Same class as the NVMe-absence
   panic filed in `specs/issues/isolation/` and the xHCI zero-HID panic M1 closed.
5. `find_hpet_base:284-291` reads `base_address_value` while ignoring the
   Generic Address Structure's `address_space` byte, which can say I/O space.

**Why now.** QEMU's tables are well-formed by construction, so none of this is
reachable from the dev host, and `specs/issues/` has no ACPI entry at
all. `iapc_boot_arch` — added for M2 so the i8042 driver can ask whether ports
0x60/0x64 are even decoded — made ACPI parsing a *load-bearing input to a
driver's decision* on the one machine whose tables nobody has read.

**Both ways.** Today, four call sites do this:

```rust
let fadt = unsafe { &*fadt_phys.as_ptr::<Fadt>() };
```

Proposed — one parse step mirroring what `xhci/device.rs:parse_config` already
does for USB descriptors (bounded walk, every field through `get`, zero length
terminates):

```rust
/// An ACPI table whose header is self-consistent: the length covers the header,
/// the checksum sums to zero, and the body is `[header_len..length]`.
/// Constructing one is the only way to read a table's body.
pub struct Table<'a> { signature: [u8; 4], body: &'a [u8] }

impl<'a> Table<'a> {
    fn parse(at: DirectMap) -> Option<Self> { … }   // length >= 36, checksum == 0
    pub fn body(&self) -> &'a [u8] { self.body }
}
```

```rust
- let madt = unsafe { &*madt_phys.as_ptr::<Madt>() };
- let entries_len = length - size_of::<Madt>();
+ let madt = Table::parse(madt_phys)?;
+ let entries = madt.body().get(size_of::<Madt>() - size_of::<SdtHeader>()..)?;
```

**Code delta.** `find_table` returns `Option<Table>` instead of
`Option<DirectMap>`, which **deletes the `unsafe { &*ptr }` at four call sites**
(`init_power:204`, `iapc_boot_arch:236`, `find_hpet_base:287`, `parse_madt:297`)
and moves the one remaining `unsafe` into `Table::parse`. The two underflowing
subtractions become slice ranges that cannot underflow. Net: one ~25-line type,
four unsafe blocks and two subtractions gone.

**Blast radius.** One file, six functions, four external callers (`main.rs`,
`ioapic::init`, `i8042::init`, `clock::init`). No ABI.

**Standing. New — no ACPI entry exists in `specs/issues/`.**

---

### F6 — Drivers destructure `KernelSlice` into `_phys`/`_ptr` pairs and carry the halves

**Location.** Counted: **37** destructuring sites
(`grep -rnE "let [a-z_0-9]+_(phys|ptr) = .*(\.phys\(\)|\.base\(\)|ptr_at)"`),
**27** struct fields that are one half of such a pair. `virtio_sound.rs:259-284`
is the clearest:

```rust
    /// Physical addresses for virtqueue descriptors.
    req_phys: u64,
    resp_phys: u64,
    /// Virtual pointers for kernel read/write.
    req_ptr: *mut u8,
    resp_ptr: *mut u8,
    /// Physical base of the TX meta region (for descriptor addresses).
    meta_phys: u64,
    /// Virtual base of the TX meta region (for kernel write_volatile).
    meta_ptr: *mut u8,
    tx_data_phys: [u64; TX_INFLIGHT_MAX],
    event_buf_phys: u64,
    event_buf_ptr: *mut u8,
```

built from `virtio_sound.rs:552-557`:

```rust
let req_phys  = ctrl_bufs.phys() + REQ_OFFSET as u64;
let resp_phys = ctrl_bufs.phys() + RESP_OFFSET as u64;
let req_ptr   = ctrl_bufs.ptr_at(REQ_OFFSET);
let resp_ptr  = ctrl_bufs.ptr_at(RESP_OFFSET);
let meta_phys = meta.phys();
let meta_ptr  = meta.base();
```

**What the code does today.** `KernelSlice` already *is* the pair — base, size,
`.phys()`, `.base()`, `.subslice()`. The drivers take one, split it into a `u64`
and a `*mut u8`, throw the size away, and then keep the two halves in sync by
naming convention. `mm/mod.rs:117` also already has `DirectMap`, the phys/virt
newtype, whose doc comment says exactly this: *"Use at the boundary between
physical and virtual — not for storing pointers."* Counted:
`grep -rn "\.phys()" kernel/src/drivers/` → **54**;
`grep -rn "DirectMap" kernel/src/drivers/` → **21**.

**Bug permitted.** A `u64` physical address and a `u64` virtual address are the
same type, and the halves are adjacent fields distinguished by a suffix.
`nvme.rs`'s `data_phys`/`prp_list`, `msc.rs`'s `data_phys`/`csw_phys`/
`scratch_phys` — all `u64`, all handed to a device that will DMA to them.
Swapping two at a call site type-checks. Nothing in the record says this has
happened, which is why the deletion is the stronger argument.

**Both ways.** Proposed: stop splitting. Keep the `KernelSlice` — which is a
`Copy` two-word value, so this costs nothing:

```rust
    req: KernelSlice,
    resp: KernelSlice,
    meta: KernelSlice,
    tx_data: [KernelSlice; TX_INFLIGHT_MAX],
    event_buf: KernelSlice,
```

```rust
let req  = ctrl_bufs.subslice(REQ_OFFSET, REQ_LEN);
let resp = ctrl_bufs.subslice(RESP_OFFSET, RESP_LEN);
```

and at use, `self.req.phys()` and `self.req.base()` where `self.req_phys` and
`self.req_ptr` were.

**Code delta.** **27 counted fields become ~14 (projected); 37 counted
destructuring lines become ~19.** The comments that exist only to say which half
a field is — `/// Physical addresses for virtqueue descriptors.` / `/// Virtual
pointers for kernel read/write.`, four of them in `virtio_sound.rs` alone — go
with them, because the type says it. And the region keeps its *size*, which is
what F8 needs and what makes F1's `len` checkable at all.

**Blast radius.** Six drivers, mechanical, no ABI. Touches `xhci/`, which is
under active edit.

---

### F7 — Interrupt vector numbers declared twice, while the correct pattern is already in the tree

**Location.** `arch/idt/mod.rs:25-38` declares the enum:

```rust
enum Vector { … Xhci = 0x21, VirtioNet = 0x22, VirtioSound = 0x23, I8042 = 0x24, … }
pub const I8042_VECTOR: u8 = Vector::I8042 as u8;   // :55
```

and three drivers declare the same numbers independently:

```
kernel/src/drivers/xhci/mod.rs:56          const XHCI_VECTOR: u8 = 0x21;
kernel/src/drivers/virtio_net.rs:37        const VIRTIO_NET_VECTOR: u8 = 0x22;
kernel/src/drivers/virtio_sound.rs:467     const VIRTIO_SOUND_VECTOR: u8 = 0x23;
```

**Bug permitted.** The IDT gate is installed from the enum
(`idt/mod.rs:323-325`); the MSI-X table entry is programmed from the driver
constant (`xhci/mod.rs:639`, `virtio_net.rs:146`, `virtio_sound.rs:485`). Change
one and not the other and the device delivers to a vector with no gate: `#GP` on
the first interrupt, attributed to whatever was running. No build error, no test,
nothing in the log until it fires.

**i8042 already does it right** — `I8042_VECTOR` is `pub` from the enum and the
driver imports it (`i8042/mod.rs:55`, used at `:751`, `:762`, `:817`). The three
older drivers were not converted.

**Both ways.** Delete the three constants; export from the enum the way
`I8042_VECTOR` already is:

```rust
pub const XHCI_VECTOR: u8 = Vector::Xhci as u8;
pub const VIRTIO_NET_VECTOR: u8 = Vector::VirtioNet as u8;
pub const VIRTIO_SOUND_VECTOR: u8 = Vector::VirtioSound as u8;
```

`xhci/mod.rs:56` becomes `use crate::arch::idt::XHCI_VECTOR;`. **Three magic
numbers deleted; the enum becomes the single definition it was written to be.**

**Same class, one line:** `arch/idt/timer.rs:49,89,93` and `arch/idt/tlb.rs`
hardcode MSR numbers `0x838` and `0x80B` in `naked_asm!`, duplicating
`X2APIC_TIMER_INIT` (`apic.rs:13`) and `X2APIC_EOI` (`apic.rs:10`).
`naked_asm!` accepts `const` operands, so these can name the same constants —
four more magic numbers gone.

**Blast radius.** Three drivers, three lines each.

---

### F8 — `KernelSlice::ptr_at` bounds-checks zero bytes, and is the escape hatch every driver uses

**Location.** `kernel/src/mm/region.rs:66-69`.

```rust
/// Pointer at offset, bounds-checked. For passing to APIs that need raw pointers.
pub fn ptr_at(&self, offset: usize) -> *mut u8 {
    self.check(offset, 0);      // asserts offset + 0 <= size
    unsafe { self.base.add(offset) }
}
```

**Bug permitted.** `check(offset, 0)` accepts `offset == size` and says nothing
about what happens next. Every use is a raw pointer the caller then offsets and
dereferences, with the length checked — if at all — by an `assert!` written
separately. The doc comment says "bounds-checked", true of the returned pointer
and false of every use of it. Sites where the length does not come from the
slice: `nvme.rs:264` (bounded by a hand-written `assert!` at `:237`),
`nvme.rs:255`/`:290` (PRP list, unbounded), `virtio.rs:191`/`:200`/`:268-282`,
`virtio_console.rs:141` (F1's out-of-bounds read).

**Both ways.** Today:

```rust
assert!(total_bytes <= MAX_DATA_PAGES * 4096);
…
copy_nonoverlapping(dma.ptr_at(OFF_DATA) as *const u8, buf.as_mut_ptr(), total_bytes);
```

Proposed — take the length, so the bound is a parameter the compiler requires
rather than a statement the author had to remember:

```rust
pub fn ptr_range(&self, offset: usize, len: usize) -> *mut u8 {
    self.check(offset, len);
    unsafe { self.base.add(offset) }
}
```

```rust
copy_nonoverlapping(dma.ptr_range(OFF_DATA, total_bytes) as *const u8,
                    buf.as_mut_ptr(), total_bytes);
```

**Code delta.** Three hand-written bounds statements become redundant —
`nvme.rs:237`, `nvme.rs:270` and `msc.rs:342`, each a `assert!(len <= CAP)`
guarding a `ptr_at` — replaced by an argument that cannot be omitted.
(`nvme.rs:236`/`:269`'s `assert!(buf.len() >= total_bytes)` stay: they are about
the *caller's* buffer, which `ptr_range` says nothing about.)

**Blast radius.** `grep -rn "ptr_at" kernel/src/` is the enumeration; five
drivers. Naturally paired with F6, which is what gives the region a size to check
against in the first place.

**Standing.** The operative half of the filed design-debt entry
"`KernelSlice::from_raw` cannot check the one thing that makes the type safe"
(`specs/issues/design-debt/`). That entry names `from_raw` and three constructors. **It does
not name `ptr_at`, and `ptr_at` is where the checking actually stops** — a
correctly-constructed `KernelSlice` still yields an unchecked pointer to anyone
who asks. Recorded as the fourth site of the same defect.

---

### F9 — `wrmsr` is a safe `fn` while `write_cr3` is `unsafe`; `invpcid` takes three `u64`s

**Location.** `kernel/src/arch/cpu.rs`. The module draws the `unsafe` line —
`write_cr3:72`, `lidt:99`, `ltr:106` are `unsafe fn` with `# Safety` sections.
On the other side of it:

```rust
#[inline]
pub fn wrmsr(msr: u32, value: u64) { … }                 // :14 — safe

#[inline]
pub fn invpcid(kind: u64, pcid: u64, addr: u64) { … }    // :84 — three interchangeable u64s
```

**Bug permitted.** `wrmsr` writes any MSR: `IA32_EFER`, `IA32_LSTAR` (the
syscall entry point), `IA32_GS_BASE` (the per-CPU pointer every GS-relative
offset in `percpu.rs` depends on), `IA32_FMASK`. Writing any of those wrongly is
memory-unsafe or worse, and the signature says safe while the strictly narrower
`write_cr3` says unsafe. The inconsistency is the defect: a reader reasonably
infers from `write_cr3`'s `unsafe` that the safe functions beside it cannot break
the machine. `invpcid`'s `kind` is an enum described in a doc comment ("Type 0:
single (pcid, addr). Type 1: all for pcid. Type 2: all PCIDs") typed as `u64`,
positionally adjacent to two other `u64`s.

**Both ways.** `invpcid` first, because it is unambiguous. Today, one caller
passes a bare integer whose meaning is in a comment three lines up. Proposed:

```rust
#[repr(u64)]
pub enum InvPcid { Single = 0, AllForPcid = 1, AllPcids = 2 }
pub fn invpcid(kind: InvPcid, pcid: u64, addr: u64) { … }
```

For `wrmsr`, the improvement is not `unsafe {}` at 21 call sites
(16 in `apic.rs`, 1 in `percpu.rs`, 4 in `syscall.rs` — counted) but the safe
per-MSR wrapper `apic.rs` is already halfway to: `eoi()`, `send_init()`,
`send_sipi()`, `arm_one_shot()`, `stop_timer()` are exactly that shape. Finishing
it leaves `enable_x2apic:30,33`, `kick_cpu:89` and `halt_all_cpus:104` behind
named functions, and `wrmsr` itself becomes `unsafe` with a `# Safety` section
naming the three MSRs that matter.

**Blast radius.** `invpcid`: one caller. `wrmsr`: `apic.rs`, `percpu.rs:337`,
`syscall.rs`.

---

### F10 — i8042 deadlines and durations are bare `u64` in two different units

**Location.** `kernel/src/drivers/i8042/mod.rs:420-470`.

```rust
fn deadline(millis: u64) -> u64 { crate::clock::nanos_since_boot() + millis * 1_000_000 }
fn stage(millis: u64, budget: u64) -> u64 { deadline(millis).min(budget) }
fn wait_writable(deadline: u64) -> bool { … }
fn read_data(deadline: u64) -> Option<u8> { … }
fn command(cmd: u8, deadline: u64) -> bool { … }
```

**Bug permitted.** `deadline` takes milliseconds and returns nanoseconds; both
are `u64`. `stage(500, budget)` takes a duration in the first slot and an
absolute deadline in the second, both `u64`. `wait_writable(500)` compiles and
means "give up 500 ns after boot", i.e. give up immediately — a silent, instant
timeout that reads as a controller fault. `clock.rs` offers only
`nanos_since_boot() -> u64` (`grep -n "pub fn" kernel/src/clock.rs`), so there is
no typed time anywhere to reach for.

**Both ways.** Today:

```rust
let budget = deadline(1500);
let selftest_deadline = stage(500, budget);
```

Proposed, in `clock.rs`:

```rust
#[derive(Clone, Copy, PartialEq, PartialOrd)] pub struct Nanos(pub u64);
/// An absolute point on the boot clock. Constructible only from `now()` plus a
/// `Nanos`, so a duration cannot be passed where one is expected.
#[derive(Clone, Copy, PartialEq, PartialOrd)] pub struct Deadline(u64);

impl Deadline {
    pub fn in_(d: Nanos) -> Self { Self(nanos_since_boot() + d.0) }
    pub fn passed(self) -> bool { nanos_since_boot() >= self.0 }
    pub fn min(self, other: Self) -> Self { Self(self.0.min(other.0)) }
}
pub const fn millis(n: u64) -> Nanos { Nanos(n * 1_000_000) }
```

```rust
let budget = Deadline::in_(millis(1500));
let selftest_deadline = Deadline::in_(millis(500)).min(budget);
```

**Code delta.** `stage` **deletes entirely** — it was `Deadline::min` written by
hand, and it is the function whose stage-summing arithmetic `specs/issues/hardware/`
already records as suspect. `wait_writable`, `read_data`, `command`,
`write_data`, `device_command`, `aux_command` all take `Deadline` and stop being
able to receive a duration.

**Blast radius.** `i8042/` (under active edit). The same types fit
`xhci/mod.rs:113`, `msc.rs:618` and `smp.rs:228`, which is where F11 needs them.

---

### F11 — xHCI's port-reset and controller-reset waits are unbounded, in a file whose other waits are not

**Location.** Counted: **18** `spin_loop()` sites in `drivers/`, of which **8**
are unbounded MMIO register polls with no deadline —
`xhci/mod.rs:701`, `:706`, `:709`, `:761`, `xhci/device.rs:195`,
`nvme.rs:400`, `:424`, `virtio.rs:406`. The other ten are lock spins
(`log_ring.rs:149`, `serial.rs:93`), a clock delay
(`panic_console/mod.rs:468`), and the waits that *are* deadline-bounded
(`wait_command`, `wait_transfer`, `wait_completion`, `submit_and_wait`).

**Bug permitted.** A port whose reset never completes hangs the boot with no
output, on a machine (the T14) whose only diagnostic channel is a screen that
stops repainting after `Boot: complete` (`specs/issues/hardware/`).

**Why this is a finding and not a style note.** `xhci/mod.rs:100-115` states the
rule and the reason: *"A device that never answers must cost that device and not
the CPU that asked it — which is the whole reason this exists, because every wait
in this driver used to be an unbounded `spin_loop`."* The `deadline()` helper it
introduces is used by `wait_command` and `wait_transfer` and by nothing else in
the file. The port reset — the first thing touched on any device, and the one
whose hardware is most likely to misbehave on an unfamiliar machine — was not
converted.

**Both ways.** Today, three times in `xhci/mod.rs` and once in `device.rs`:

```rust
loop {
    let ps = op_base.read_u32(portsc_off);
    if ps & PORTSC_PRC != 0 { break; }
    core::hint::spin_loop();
}
```

Proposed:

```rust
op_base.wait(portsc_off, deadline, |ps| ps & PORTSC_PRC != 0)?;
```

with, on `Mmio`:

```rust
/// Spin on `off` until `pred` holds. A device that never answers costs the
/// deadline, never the CPU.
pub fn wait(self, off: u64, until: Deadline, pred: impl Fn(u32) -> bool)
    -> Result<u32, Timeout>
```

**Code delta.** 8 counted four-line polls become 8 one-line calls plus one
~10-line helper: **≈22 lines go (projected)**, and each of the 8 gains a bound
it does not have. It is also what stops the file's own `USB_TIMEOUT_NS` comment
from being false about the file it is in.

**Blast radius.** `xhci/`, `nvme.rs`, `virtio.rs`. Depends on F10's `Deadline`.

**Standing.** Not filed. Distinct from the two filed xHCI first-boot risks (xECP
handoff, hotplug) and from the filed `submit_and_wait` spin in `virtio.rs`, which
`specs/issues/panic-path/` records for the *panic* path only.

---

### F12 — `static mut DMA_HOLDER` in `virtio_console.rs`

**Location.** `virtio_console.rs:77`. `grep -rn "static mut" kernel/src/` returns
**4**: this one, `console_mut()`'s signature on the next line, and two in
`mm/alloc.rs`'s early bump allocator.

```rust
static mut DMA_HOLDER: Option<DmaPool> = None;
```

**Bug permitted.** Very little today — written once before SMP, never read. It is
here because it is the only `static mut` in the driver tree while the file
immediately below it uses the modern idiom for the same job
(`ConsoleCell(UnsafeCell<MaybeUninit<VConsole>>)` with an `unsafe impl Sync`), so
the change is to make two things in one file consistent rather than to invent
anything. The `unsafe` block at `:169` also produces a `static_mut_refs` lint on
current toolchains; the fork estate's `--cap-lints allow` hides that class
elsewhere, but this crate is a path dependency and does not.

**Both ways.** `static DMA_HOLDER: DmaCell = DmaCell(UnsafeCell::new(None));`
with the same `unsafe impl Sync` justification the neighbouring `ConsoleCell`
carries. **Two lines.**

---

## 5. Adjacent observations, recorded but not ranked

Each is real; none passes either bar on its own.

- **`exceptions.rs:317-321` writes raw to port `0x3F8` with no `uart_present()`
  gate**, inside `debug_handler`. `serial.rs:25-32` exists precisely because a
  machine with no SuperIO reads `0xFF` from that port, and `specs/issues/panic-path/`
  records that `panic_raw_uart`'s equivalent gap "is closed now". This one is
  not. `#DB` only, so it fires only under a hardware watchpoint.
- **`ioapic::set_masked` (`:311-318`) does not read back**, while `route`
  (`:300-307`) does, in the same file, for the reason its own
  `RouteError::Readback` doc states. `i8042::quarantine:384` counts
  `set_masked(...).is_ok()` and prints it as `masked=N`, so the count means "the
  call returned Ok", not "the line is masked" — the exact distinction `09f40be`
  was about.
- **`nvme.rs` allocates a command id it never checks.** `alloc_cid:154-158` runs
  before every command; `wait_completion:105-119` never compares `cq.cid` against
  it. Sound today because every submission is synchronous, and the same
  identity-in-the-type shape as F1. Deserves a comment saying the cid is
  decorative until the queue goes asynchronous.
- **`Vector::from_raw` (`idt/mod.rs:41-50`) panics on any vector it does not
  know**, and is reached from `ExceptionContext::vector()` (`exceptions.rs:92`),
  i.e. from inside `crash_report`, which `exceptions.rs:107` declares must be
  panic-free. `trap_dispatch` filters first, so it is unreachable; the DESIGN
  RULE comment does not say so.
- **The GS-relative offsets in `asm!` string literals** — `gs:[240]` in
  `device_irq.rs:39`, `timer.rs:97-98`, `tlb.rs` — are hand-written, while
  `percpu.rs:171-184`'s 14 `const _: () = assert!(offset_of!(…))` lines exist to
  catch exactly that drift. `const` operands would let the asm name the same
  `offset_of!`. Not ranked because the asserts already catch what matters.
- **`i8042` decodes wire formats through a well-typed `toyos-ps2`
  (`KeyOutcome`, `MouseOutcome`) and then converts back to bare `u8` usage
  codes** at `keyboard::handle_key(usage, pressed)`. Out of scope (the boundary
  is `kernel/src/keyboard.rs`), noted because it is where the one typed decode in
  the tree loses its types.

---

## 6. Examined and deliberately **not** flagged

Naming a defect requires naming the bug it permits or showing the code reads
better. These were written out and dropped, with why.

**A general typed-register layer.** §3, decided on the snippets: the call sites
come out the same length and the register definitions are pure addition, against
a class with zero recorded instances. The one register where the typed form does
read better is PORTSC, and that is called out inside §3.

**`serial.rs`'s `PORT + 1` … `PORT + 5` and the `0x20`/`0x01` LSR masks
(`:35-45`, `:120`, `:360`).** Written both ways, `LSR`/`LCR`/`MCR`/`FCR`
constants replace eleven `PORT + N` expressions with eleven `PORT + LSR`
expressions — same length, no deletion, and the 16550 is the most stable
register layout in computing. The block is 12 lines, runs once, and the loopback
probe verifies the result end to end. It reads slightly better and prevents
nothing; not worth a finding, and worth doing if someone is in the file anyway.

**`#[repr(C)]` device structs read straight out of DMA** (`nvme.rs:23-32`
`IdentifyNamespace`, `virtio_sound.rs:141-150` `VirtioSndPcmInfo`,
`virtio_gpu.rs:93-96` `RespDisplayInfo`). The brief asks where a raw view is used
instead of a parsed type; for these three **the parse step is present and is the
interesting code**: `nvme.rs:207-229` refuses a sector size it cannot serve,
`virtio_sound.rs:62-99` `choose_params` refuses a device offering nothing
implementable and names the missing capability, `virtio_gpu.rs:639-650` clamps
EDID's answer. `xhci/device.rs:62-187` `parse_config` and `xhci/msc.rs:690-738`
are the model. Flagging the struct while ignoring the parse would be counting the
shape and not the property.

**`unsafe impl Send` on nine driver controllers.** Each has a stated
justification and each is protected by a `Lock` or a single-owner argument.
Nothing in the record contradicts one.

**`enum Vector`, `CpuFaultState`, `HidType`, `BufDir`, `Bot`, `Scsi`, `Host`,
`Trigger`, `Polarity`, `Function`, `IrqSource`.** Already right.

**`DmaPool` lacking RAII for its mapping.** It owns `Vec<PhysPage>`, which frees
on drop, and every pool is a boot-time `static` that never drops. There is no
paired unmap to forget.

**`BackendGuard`, `IrqGuard`, `RingGuard`, `LockGuard`.** The scope's paired
operations are already guards. The one genuinely unpaired pair —
`panic_console::detach()` / `rearm()` (`panic_console/mod.rs:190-200`) — is
deliberately not a guard, and the filed `gpu::set_resolution` entry
(`specs/issues/design-debt/`) says why: the caller must survive the window even if the callee
refuses, which a `Drop` cannot express. Making it a guard would be worse.

**`SharedToken` as a bare `u32`.** Filed (`specs/issues/design-debt/`). Not re-filed.

**`i8042`'s atomics-instead-of-a-lock byte ring** (`:126-173`). The module doc
gives the full argument: no `Lock`, no allocation, no `log!`, no wake, no
unbounded loop, single producer by pinning delivery to one CPU. This is an ad-hoc
atomic protocol and it is the correct one; the prohibitions are load-bearing and
stated.

**`as` casts on device-derived values.** Swept for silent truncation. Those that
exist are either bounded by a preceding check (`xhci/mod.rs:690-691` masks both
scratchpad fields with `0x1F`, and `Layout::new`'s doc computes the resulting
ceiling; `msc.rs:210`'s `lba32` is guarded by `bring_up`'s `last_lba > u32::MAX`
refusal at `:726`) or are narrowing a field to its own declared width. **The one
exception is F1's `id as u16`**, which is why it is F1.

---

## 7. Three cited files are not at HEAD

Checked after the audit was written, with `git cat-file -e HEAD:<path>`, because
`specs/issues/isolation/`'s postscript says to: *"In a tree with six agents committing, the
working tree is somebody's uncommitted opinion. `git show HEAD:<path>` is the
arbiter."* That warning is usually about a finding looking **fixed** by
work-in-progress on disk. This is the mirror case — a finding looking
**supported** by it.

**Not at HEAD** (untracked, another agent's in-flight work):
`kernel/src/drivers/xhci/msc.rs`, `kernel/src/drivers/usb_storage.rs`,
`kernel/src/usb_gate.rs`.

**At HEAD, verified:** `block.rs` (and `read_blocks` still returns `()` there —
`git show HEAD:kernel/src/block.rs:14`), `nvme.rs`, `page_cache.rs`,
`virtio.rs` (`DescSlot(id as u16)` at `HEAD:virtio.rs:368`), `acpi.rs`, and all
three `setup_msix` functions.

What this changes, precisely:

- **F1, F2, F5, F6, F7, F8, F9, F10, F12 stand at HEAD unaltered.** F1's
  out-of-bounds read is in `virtio_console.rs`; F2's three copies are all at
  HEAD; F5's underflows are at HEAD.
- **F3 stands at HEAD on the NVMe half only.** The trait and its `()` return are
  at HEAD, all six discarded NVMe statuses are at HEAD, and `page_cache.rs:235`'s
  stale-slot consequence is at HEAD. The `usb_storage.rs` half — the second
  implementation, its `healthy()` workaround, and the `usb_gate.rs:90`
  fail-safe-by-accident analysis — is about uncommitted code. It strengthens the
  finding and is not what the finding rests on.
- **F11 loses one of its eight sites at HEAD**: `xhci/device.rs:195` is in a
  file that *is* at HEAD, so all eight stand; but the `msc.rs` waits I checked
  as already-bounded are not, so "the other ten are deadline-bounded" is a claim
  about the working tree.
- **`msc.rs` is cited four times as the exemplar** — in §2, §3, F1's asymmetry
  argument and §6's parse-step defence. Those citations are to code that is real
  and readable today and is not in `main`. If it never lands, §6's "the parse
  step is present and is the interesting code" loses one of its three examples
  (`nvme.rs:207-229` and `virtio_sound.rs:62-99` are both at HEAD and both
  survive), and F1's "four consumers, three standards" becomes an argument from
  `virtio_sound.rs`'s two `assert!`s alone, which is weaker but still true.

Nothing here was re-derived against HEAD beyond the checks above; the audit was
read against the working tree, which is the tree the next person will also see.

---

## 8. Method, and what was not done

Read in full: all 17 files under `kernel/src/drivers/` and all 16 under
`kernel/src/arch/` except `syscall.rs` (1,925 lines, skimmed for the audited
patterns only — it is the ABI boundary, has had its own hardening passes, and its
findings would duplicate `specs/issues/isolation/`). Also read for context: `mm/mod.rs`,
`mm/mmio.rs`, `mm/region.rs`, `block.rs`, `page_cache.rs` and `usb_gate.rs` (read
paths), CLAUDE.md, `specs/issues/` in full, `specs/metal-track-history.md`
in full.

**Nothing was built or run.** No `cargo build`, `cargo test` or `cargo run` —
five other agents share this checkout. Every quantitative claim about existing
code is a `grep` or `wc` whose command is written beside it; every number about
code that does not exist yet is marked **(projected)** and came from writing the
replacement out.

Claims about *behaviour* — F1's out-of-bounds read, F3's stale-page-served — are
read off the code and are stated as what the code permits, not as reproductions.
**Neither has been staged**, and F1 in particular needs a device that misreports
a used-ring element, which QEMU will not do — the same structural limit
`specs/device-test-strategy.md` describes, and the reason both are argued by
construction rather than by a hit.

`xhci/` and `i8042/` were audited as of this tree and are under active edit.
F4's i8042 line numbers will move; the finding is the 14-of-16 ratio and the
missing `#[must_use]`, not any line.

**Sequencing** (dependency order and file contention only, not effort):
F7 and F12 touch nothing else. F1 and F2 are independent of everything.
F10 must land before F11, which needs its `Deadline`. F6 must land before F8,
which needs the region to still carry its size. F3 and F5 are independent but
both propagate `Result` outward, so each wants its own commit and its own test.
F4 and F10 both touch `i8042/`, which another agent holds today.
