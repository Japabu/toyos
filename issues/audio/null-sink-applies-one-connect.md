---
status: open
kind: defect
opened: 2026-08-06
task: 172
---

# The null sink's mix loop applies exactly one connect and then stops, on the T14 only

Read off two T14 boots (`boot7-usba-doom.log`, `boot5-doom-wedge.log`). The
shape is the same in both and it is narrow:

- soundd finds no device and presents the null sink; `null sink idle` prints.
- The first client connects. The control thread prints `opening stream`, the
  **mix loop** prints `client 0 connected (id=0)` — so it woke, drained the
  command pipe and applied the command.
- Nothing from the mix loop ever again. `stats.report` fires every 2 s while a
  client is present and the connect resets that window, so a single missing
  stats line is proof the loop stopped; boot7's client was a 2 s tone and no
  line follows it.
- Every later client is stranded: the control thread keeps running (it printed
  `opening stream` for doom 14 s later) and `open_stream` answers, but no
  `client N connected` follows, so the mixer never signals it and it blocks
  reading its signal pipe forever. That is `tone` never exiting and doom
  wedging with a black window before its render loop.

**Not reproduced in QEMU.** `tests/desktopaudiocase` was built for this: the
T14's shape, with the client's three descriptors as pipes to a terminal that is
a compositor surface — the fidelity gap `metal_sim_null_audio` and
`null_sink_shipped_client` both have, since both spawn the client from a test
binary whose stdio is the console. Green at `smp=2` and at `smp=8` (the T14's
count), with one client, with two overlapping clients under two terminals, and
with a terminal opened afterwards.

**Eliminated, each with a run behind it.** CPU count. The cpal client path
(`null_sink_shipped_client`, adopted from `wt/toyos-hdaprobe` `fa47241`, two
`/bin/tone` in series at 1.16 s and 1.15 s). soundd blocking on a client
(`signal_clients` uses `write_nonblock`; there is no blocking write in the mix
loop). The accept path being held by a stuck client (accept and mix are
separate threads and the control thread ran for 14 s afterwards). A CQ overflow
(`Poller::wait`'s `dropped` assertion would have killed soundd, and soundd is
alive). A panic anywhere in soundd, for the same reason.

**Eliminated by reading, and recorded so it is not re-derived.** The mix loop
holds no lock across its wait, and neither `PIPES` nor `IO_URINGS` was held on
the T14 at 47 s — the control thread took both to accept doom's connection and
open its stream. The mixer's timeout while streaming is finite (one device
period), so a park with that deadline is the only way to stop, and the timer
that ends it is the same one the compositor's frame interval rides. The
closed `io_uring::cancel_by_source` lost wake is *not* this: the mix loop's only
registration is on the command pipe, which soundd owns both ends of and nobody
closes.

**What settles it on the next T14 boot: Ctrl+Alt+D on the wedged machine.** The
dump is machine-wide and process-named now (`issues/diagnostics/`), so one press answers the split
directly. soundd's mix thread parked with a sane deadline says the timer did
not fire; parked with an absurd one says the timeout was computed wrong; absent
from every parked list says nothing ever held it, which would move this into
#142's family rather than audio's. The report paints the panel, so the machine
with no serial port answers on glass and a photograph is enough.

Until that press happens, the three gates this task landed are what stands
between the milestone and a silent recurrence: a client through the null sink
must exit, a second must be taken up while the first streams, and the desktop
must still answer afterwards.
