---
status: open
kind: defect
opened: 2026-08-08
---

# `poll_used` returns a device-chosen descriptor id and length, both unchecked

`Virtqueue::poll_used` (`kernel/src/drivers/virtio.rs:387-399`) reads `id` and
`len` out of the used ring — memory the *device* writes, and for virtio-net
memory *netd* writes too (`netd-writable-virtqueue`) — and returns `DescSlot(id as u16)` and
`len` with no comparison against `self.size` or against any buffer length.
`UsedRingConsumer::poll` (`:191-205`) does the same for `id`. `DescSlot` is this
codebase's own proof token, deliberately non-`Copy` and non-`Clone`
(`:169-172`), and it proves the descriptor is *free*; it says nothing about the
number being in range, and `id()` is public.

**Three consequences in the code today.**

- **An out-of-bounds read, in `unsafe`, from an unchecked length.**
  `virtio_console::try_read_byte_locked` (`virtio_console.rs:132-148`) stores the
  returned `len` and walks `*c.rx_ptrs[p.buf_idx].add(p.pos as usize)` until
  `pos >= len`. `RX_BUF_SIZE` is 256 (`:29`) and the eight RX buffers sit 256
  bytes apart inside one page (`:186-191`), so a `len` above 256 walks the next
  buffer, then `OFF_RXVQ`'s virtqueue rings, then past the pool — all inside the
  direct map, so it faults nowhere and delivers kernel memory to the console as
  typed input.
- **A kernel panic from an index.** `desc_to_rx` is `[u8; 16]`
  (`virtio_console.rs:56`) and `desc_to_buf` is `[u16; 256]`
  (`virtio_net.rs:59`), and `slot.id()` ranges over the whole `u16`. Rust bounds-
  checks both, so the failure mode is a panic rather than a read — and on the
  virtio-net side the value comes out of a page netd writes, which makes it a
  userland-triggered kernel panic, the thing CLAUDE.md's corollary forbids by
  name.
- **A frame length nothing bounded.** `poll_rx` returns
  `written_len as usize - NET_HDR_SIZE` (`virtio_net.rs:90-101`) and
  `SYS_NIC_RX_POLL` packs it as `((buf_idx as u64) << 16) | (frame_len as u64)`
  (`kernel/src/arch/syscall.rs:482`) with no mask, so a length above 65535
  corrupts the buffer-index field of the word netd unpacks, and netd's own
  `rx_buf(idx)` walks off its mapping.

**One consumer of the four does check.** `virtio_sound::drain_tx`
(`virtio_sound.rs:110-118`) rejects a head that is not a chain's and counts it as
`stray` rather than trusting it; its comment at `:100-102` states the rule —
"a head that is not a chain's is untrusted input, not a device fault". virtio-net,
virtio-console and virtio-gpu do not, and virtio-gpu's `submit` masks a bogus id
with `% size` (`virtio.rs:346`) so it silently aliases another submission's
descriptors instead of failing.

**Standing.** Audited and explicitly never filed: the kernel-drivers type-safety
audit's F1 carried the full analysis and the
proposed fix at the primitive — `submit` records each chain's byte total, and
`poll_used` answers `None` on an id past `size` or a length past the chain — and
closed with "**Standing.** … Not filed." This is that filing. Two of
its citations had already drifted: `virtio.rs:161-164` is `:169-172` today, and
the two `virtio_sound.rs` `assert!`s it named as the counter-example are gone,
replaced by the `stray` counter above. No test covers any of it.
