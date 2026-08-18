---
status: open
kind: defect
opened: 2026-08-08
---

# The NIC's virtqueue is inside the page netd is granted writable, so netd can aim the device at any physical address

`virtio_net::init` registers **the whole 2 MiB `DmaPool` page** as one shared
region (`kernel/src/drivers/virtio_net.rs:208-209`) and `device::try_claim`
grants it to whoever claims `DeviceType::Nic` (`kernel/src/device.rs:136-141`).
Every shared mapping is writable — `SharedRegion::map_into` passes
`writable = true` with no alternative (`kernel/src/shared_memory.rs:64`,
`mm/paging.rs:534-556`) — and netd maps the full 2 MiB
(`userland/netd/src/main.rs:47`).

**What is in that page besides buffers.** The layout
(`virtio_net.rs:29-37`) puts the RX descriptor table at offset 0, the RX avail
ring at `0x1000`, the RX used ring at `0x2000` and the entire TX virtqueue —
descriptors, avail and used — at `0x3000`; only from `0x4000` on is it the
buffers netd is meant to have. Derived from the constants in that file: 4096 +
516 + 2052 bytes for the RX rings, 256 + 36 + 132 for the TX ones, **7088 bytes
of virtqueue control structure, the last of it at `0x31a7`**, all of it inside
the window. `NicInfo` (`toyos-abi/src/net.rs`) tells netd only
`rx_buf_offset`/`tx_buf_offset`; the *mapping* is not bounded by what it was
told.

**Three primitives follow, in severity order.**

1. **An arbitrary physical write.** Each of the 256 RX descriptors carries a
   `u64` physical address the device will DMA the next frame into. All 256 are
   posted at init (`virtio_net.rs:234-237`) and stay posted until frames arrive,
   so at rest netd holds 2048 bytes of live DMA targets that the device has not
   read yet. Rewriting one aims the NIC at any physical address in the machine —
   kernel text, page tables, another process. `refill_rx` rewrites the
   descriptor from `rx_phys[buf_idx]` on the *next* refill (`:68-78`), which
   closes nothing: the device's write happens first, and the frame contents are
   the attacker's too. Nothing else stands in the way — `kernel/src/iommu/mod.rs:11`
   says of itself that "this module *refuses nothing*", every function is in one
   identity-mapped domain, and stages I0–I2 are all that is built.
2. **Kernel memory onto the wire.** The TX descriptor at `0x3000` is written by
   `submit()` and read by the device. Same window, opposite direction: a
   rewritten `addr`/`len` reads arbitrary physical memory out through the NIC.
   Narrower than (1) as a race — under TCG QEMU services the notify inline — but
   it is a race only because of how the host schedules, not because of anything
   the kernel does.
3. **Forged completions.** The RX used ring at `0x2000` is what
   `Virtqueue::poll_used` reads back. See `poll-used-descriptor-unchecked` for what the kernel
   then does with an `id` and a `len` it did not check; that entry's "a buggy or
   malicious *device*" becomes "netd" here.

**The fix shape is already in this tree, one file over.** `virtio_sound::init`
allocates *two* pools (`virtio_sound.rs:374-375`): `dma_kernel` holds the
descriptor tables and the TX used ring and is never registered; `dma_shared`
holds the avail rings and the buffers and is the only token soundd is given
(`:429-433`). A forged avail entry can then only name a chain the kernel itself
built, and the comment at `virtio_sound.rs:403-405` states the rule for the used
ring in as many words. virtio-gpu registers only its framebuffer and cursor
pages and keeps its `DMA` pool private (`virtio_gpu.rs:464-479`, `:678`).
virtio-net is the one device that hands its virtqueue out, and it predates both.

**Standing.** No test stages a netd that writes outside its buffers, and none
can while the mapping is one 2 MiB grant. `specs/assessments/type-safety-audit/kernel-drivers.md`
F1 audits the *reading* half (`poll-used-descriptor-unchecked`) and does not reach this: it
argues from a misbehaving device throughout, which is a hardware-failure
argument, where this is a process-isolation one. Splitting the pool the way
virtio-sound does is the whole fix and needs no ABI change — `NicInfo` already
addresses everything by offset.
