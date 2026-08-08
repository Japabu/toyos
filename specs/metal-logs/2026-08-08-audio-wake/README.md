# The T14's audio tail, and the diagnostic build that was 80× of it

Two boots of the owner's ThinkPad T14 Gen 2 on 2026-08-08, same tree, differing
only in whether the kernel was built with `--kernel-feature heartbeat`. Both
played tones from `/bin/tone` on the HDA path with a full desktop up.

| file | kernel | soundd stats lines |
|---|---|---|
| `2026-08-08-150958-heartbeat.log` | `heartbeat` (and so `diag-tick`) | 67 |
| `2026-08-08-153139-clean.log` | shipping | 15 |

## What the pair measures

`max_wake_lat_us` is soundd's **worst single wake in that ~2 s window** — a
per-window maximum, not a typical wake. One device period is 128 frames at
44100 Hz = 2.902 ms and the ring is 8 of them = 23.219 ms, so a wake later than
23.219 ms guarantees the device ran out of audio. `drains` counts exactly that.

Over the streaming windows of each boot:

| | `heartbeat` | clean |
|---|---|---|
| median per-window worst wake | 50,512 µs | **1,307 µs** |
| min | 24 µs | 485 µs |
| max | 80,133 µs | 63,225 µs |
| drains | 375 | **5** |
| underruns | 522 | 88 |

The first column is not a measurement of the shipping kernel. `heartbeat`
carries `diag-tick`, which caps how long an idle CPU may sleep, and it emits
two log lines every 250 ms; `/log` on this machine is the SanDisk Ultra the
machine boots from, and every batch of log lines ends in a `sync_mount` of it.
The instrument was four USB filesystem syncs a second, and it was measuring
itself.

## What the clean boot says

Windows with `max_batch=1` — soundd taking one completion per wake, which is
the healthy shape — report a worst wake of **485–514 µs**, a sixth of a period.
The wake path is not slow.

What is left is a tail of six events, and every one of them follows a burst of
kernel log lines:

| line | `max_wake_lat_us` | `max_batch` | immediately preceded by |
|---|---|---|---|
| 280 | 39,411 | 8 | `sched:`/`PMM:` health burst, 13 lines, t=21.241 |
| 292 | 14,121 | 6 | `exit: tone` burst, 5 lines, t=23.742 |
| 345 | 55,391 | 8 | `sched:`/`PMM:` health burst, 13 lines, t=41.253 |
| 347 | 30,950 | 8 | the same burst's flush, spilling into the next window |
| 359 | 39,194 | 8 | `exit: tone` burst, 8 lines, t=44.668 |
| 428 | 63,225 | 8 | `sched:`/`PMM:` health burst, 13 lines, t=61.257 |

`scheduler::log_health` emits that 13-line burst every `SNAPSHOT_INTERVAL_NS`
= 10 s. Three of them appear in this boot, at 21.241, 41.253 and 61.257, and
each is followed by the worst wake of the session.

## The mechanism

`sched::driver::idle_loop` runs `log_file::poll()` **before** `pass()`. A burst
of log lines makes `file_has_pending()` true; the next CPU round the idle loop
appends them to `/log/<boot>.log`, writes the file back and syncs the mount.
Every one of those steps is a USB bulk transfer, and `xhci::wait_transfer`
spins for the whole of it with the controller lock held and preemption
disabled. That CPU reaches no scheduler pass until the flush ends.

The CPU it happens on is not incidental. A task parks on the CPU that has
nothing else to run, so the CPU soundd is waiting on is precisely the CPU that
reaches the idle loop first and takes the flush — and both soundd's completion
wake (`irq_ring` records are drained only by the CPU that took the interrupt)
and its parked deadline (armed on its home CPU) are stranded there for the
duration.

`max_batch=8` on every one of these windows is the signature: soundd came back
to find the whole ring completed at once.

## Reading these files

Both are the kernel's own `/log` file, which is the only channel this machine
has — it has no serial port (`serial: 16550 loopback read 0xff`). Userland
lines carry no timestamp; their position between two `[kernel <t>]` lines is
what dates them.
