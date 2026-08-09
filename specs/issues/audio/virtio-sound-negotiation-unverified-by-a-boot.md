---
status: open
kind: finding
opened: 2026-08-01
---

# The virtio-sound negotiation fix was never verified by a boot

`choose_params` (`virtio_sound.rs:62`) selects a rate and channel count the
device actually advertises, and a device offering nothing this driver implements
logs *which* capability is missing and leaves the machine to boot without audio,
rather than being silently remapped to 44100/2. Landed at `4fce59c`.

**Not verified by a QEMU boot.** The change is to device negotiation, and seven
consecutive boot attempts died in shared-toolchain contention
(`x-py-by-hand-takes-no-lock`). The reasoning that it still selects (44100, 2) on
QEMU is *static*, read off an earlier boot log's advertised bitmaps.
`cargo test -- audio` on a quiet tree is owed before this is treated as proven.
Recorded as a live gap because an unverified change to negotiation is exactly the
kind that fails on the one machine nobody booted.
