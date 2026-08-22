---
status: open
kind: track
opened: 2026-08-22
---

# `DmaPool` hands out raw access where it should hand out a view

`DmaPool::slice()` returns a `KernelSlice`, whose every accessor
(`read`, `write`, `as_slice`, `copy_from`, `zero`) is an `unsafe fn`. So
every driver that touches its own DMA memory spells the unsafety at the
call site, and the `undocumented_unsafe_blocks` sweep of `drivers/`
(2026-08-22) found that this one shape is most of what the area's `unsafe`
is.

**Measured after the sweep**: 72 `unsafe` blocks remain under
`kernel/src/drivers/`, down from 132. **35 of them are this** — a
bounds-checked view over `DmaPool` memory, read or written through an
`unsafe fn` because that is the only accessor there is. They are:

| file | blocks | what they are |
|---|---:|---|
| `kernel/src/drivers/mod.rs` | 1 | `DmaPool::alloc`'s `KernelSlice::from_raw` |
| `kernel/src/drivers/virtio.rs` | 3 | `Ring::read`, `Ring::write`, `Ring::zero` |
| `kernel/src/drivers/nvme.rs` | 8 | `NvmeQueue::submit`, the two `wait_completion` reads, `zero_dma`, `fill_prp_list`, the Identify read, the two data copies |
| `kernel/src/drivers/xhci/mod.rs` | 6 | `zero_dma`, `write_dcbaa`, `TrbRing::put`, `next_event`, `write_ctx32`, `endpoint_state` |
| `kernel/src/drivers/xhci/wait/msc.rs` | 4 | `read_dma`, `write_dma`, the CBW write, the CSW read |
| `kernel/src/drivers/xhci/wait/boot.rs` | 2 | the scratchpad array, the ERST entry |
| `kernel/src/drivers/xhci/hid.rs` | 2 | the report copy, `stage_break` |
| `kernel/src/drivers/xhci/device.rs` | 1 | `read_back`'s scratch-page byte view |
| `kernel/src/drivers/virtio_console.rs` | 2 | the TX copy, the RX byte read |
| `kernel/src/drivers/virtio_gpu.rs` | 2 | `put`, `answer` |
| `kernel/src/drivers/hda.rs` | 2 | the BDL/PCM zeroing, the BDL entry writes |
| `kernel/src/drivers/virtio_net.rs` | 1 | the RX header zeroing |
| `kernel/src/drivers/virtio_sound.rs` | 1 | the shared window's zeroing |

The other 37 are not this: the panic console's `UnsafeCell`s and its writes
to a GOP framebuffer (17), `virtio_sound`'s completion-record ring and ISR
consumer cell (6), `hda`'s stream-ISR cell (3), `virtio_console`'s
`ConsoleCell` and its two accesses (3), `serial`'s two `asm!`s and
`panic_flush` (3), `acpi`'s two reads of a firmware-supplied physical
address (2), `xhci/wait/mod.rs`'s frame-pointer read (1), and the two
`unsafe impl Send`s that survive because their structs hold typed
queue-entry pointers (`NvmeBlockDevice`, `XhciController`) (2).

## What to build

A typed view handed out by `DmaPool` itself, in the shape `mm::Mmio`
already has for MMIO: private construction, safe accessors, and every
access bounds-checked for `size_of::<T>()` rather than for the offset
alone. The sweep built five driver-local approximations of exactly this —
`virtio::Ring`, `xhci::zero_dma`, `xhci::wait::msc::{read_dma, write_dma}`,
`nvme::zero_dma`, `virtio_gpu::{put, answer}` — which is the argument that
it belongs on `DmaPool` once.

Two things it has to get right, both of which the sweep ran into:

- **The length, not the offset.** `KernelSlice::ptr_at` asserts
  `offset <= size` and nothing about what is read through the pointer, so a
  `u32` read at `size - 1` runs three bytes past the region. Every
  accessor must go through `subslice` or an equivalent. The sweep closed
  this at each site it touched; a view closes it by construction.
- **Volatile is not optional and not universal.** A descriptor ring, a
  completion queue and a device context race the device by design and must
  be volatile; a CBW's fields are unaligned by the spec's own layout and
  must be `read_unaligned`/`write_unaligned`. A view with one accessor
  cannot serve both — `xhci/wait/msc.rs` needs the unaligned one and
  `xhci/mod.rs` the volatile one, in the same driver.

## What it is blocked on

Nothing but the owner's word that a *safe* accessor over DMA memory is the
right claim. It is the claim `Mmio::read_u32` already makes for MMIO, and
the residual is the same one `KernelSlice::from_raw` carries today
(`issues/design-debt/kernelslice-from-raw-cannot-check-itself.md`): the
type cannot check that the region it names outlives it. Making the view
borrow the pool rather than copy out of it would close that too, and is
the larger half of this work — `DmaPool::slice()` returns a `Copy`
`KernelSlice` with no lifetime, which is why `virtio_console` and
`virtio_net` need a `static` holding the pool alive at all.
