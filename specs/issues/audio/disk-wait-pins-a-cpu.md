---
status: open
kind: defect
opened: 2026-08-08
---

# The audio pops are four spinlocks, not one spinning driver

**`specs/plans/blocking-io-plan.md` is the finding, the evidence and the wave that
closes it.** What belongs here is the sentence an agent must not miss and the
two commands.

Measured 2026-08-08 on `wt/toyos-asyncusb` at `87835d1`: at the moment a disk
transfer is waited for, an ordinary guest is **four ticket spinlocks deep** —
`log_file::SINK`, `vfs::VFS`, `fat32_adapter::VOLUMES` and `xhci::XHCI` — and
every one of them disables preemption for its whole life. `io-depth-probe`
(kernel feature) reports the depth with the backtrace that got there: 4 from the
idle loop, 5 from a syscall. So **making `xhci::wait_transfer` park cannot work
and would not help**: `prepare_wait` asserts the depth equals the context's
baseline, so a park there is a named panic on the first flush — and the three
locks above `XHCI` would still hold the CPU if it were not. `fd.rs:644` puts
every userland file write-back at the same depth, so this is not a property of
the log sink; it is that this kernel cannot touch a disk without pinning a CPU
for the whole device round trip. The log sink is only the writer that runs
continuously, which is why it is the one gate A sees.

`usb-slow-device` (kernel feature) holds every mass-storage bulk completion back
2 ms, which is what a USB stick's erase block does to a 4 KiB write and what
QEMU's `usb-storage` has no device, drive or machine property to express.
`cargo test --test toyos-build -- audio_tone --slow-usb` stages the T14's harm
on this host: soundd's worst wake goes 7,117 → 165,948 µs at smp=1 and
10,632 → 259,706 µs at smp=8 — 7 to 11 whole 23.2 ms pipelines — drains appear
on three of the four configs, and one boot of three submitted 76 silent periods
and tripped gate A's own harm verdict. Baseline arm at host load 5.0–6.6, slow
arm at 1.3–1.5, so the direction is not the host's. Both arms, one session, one
tree.

[`client-cpu-takes-the-log-flush.md`](client-cpu-takes-the-log-flush.md) and
[`stop-the-device-voice-keep-the-wake.md`](stop-the-device-voice-keep-the-wake.md)
are the fixes that came before. Each was right about its own defect; both chose
which CPU absorbs a stall whose duration neither touched.
