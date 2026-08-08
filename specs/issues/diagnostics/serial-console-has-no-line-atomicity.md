---
status: open
kind: defect
opened: 2026-08-07
---

# The serial console has no line atomicity, so a stats line can be cut in half by another writer

std's `Stderr` is unbuffered, so one `eprintln!` reaches `SYS_WRITE` as several
calls, one per format fragment. `SerialWriter::console` buffers a *single*
write and commits on drop (`kernel/src/drivers/serial.rs:303-319`), which makes
each fragment atomic and a line not. Two instances in one 2110-line capture,
2026-08-07:

```
netd: MAC soundd: ready, 52:54:8 buffers, 00:12:34:56
44100Hz 2ch, 512 bytes/period, 128 frames/period
netd: ready, at most 42 piped connections (4 MiB each of 1356 MiB total)
```

```
soundd: client [kernel 351.736 cpu0 tid=0] syscalls: pid=46 total=28 …
40 removed
```

The second is the kernel's own ring landing inside a userland line, so this is
not only a userland-vs-userland race. On the T14 the log *is* the instrument,
and this capture shows what that costs. Counting soundd's client lifecycle in
it gives 45 connects and 44 removes, and the one id with no removal is 40 —
whose removal is the split line above. A reader auditing that log for leaked
clients finds one, and it is not there:

```
$ grep -c 'connected (id='            45
$ grep -cE '^soundd: client [0-9]+ removed$'   44
```

Cheap in principle: give the toyos `Stderr` a line buffer, or have
`write_user` hold the ring across one logical line. Neither is free of
tradeoffs — a line buffer changes flush ordering against the kernel's ring —
so it is filed rather than fixed.

**It reds landings, which is new.** `landing-1786130703-71774.log` (2026-08-07
21:32) failed the gate on a **documentation-only branch**: `hda_tone` wants
`soundd: hda codec0 vendor=1af4` and the console carried

```
soundd: hda codec[kernel 0.262 cpu1] i8042: armed at 184ms, idle at 262ms, …
0 vendor=1af4 device=0012, 1 function group(s)
```

The splice fell between `codec` and `0`. `Serial::interleaved` named it in the
failure message, which is that instrument working — but the run is still red,
and re-running is the only recourse. soundd's next two needles a few
milliseconds later matched intact, so which line is hit is chance, and every
`must_say` naming userland output carries this rate.

That is a **third** fix candidate, harness-side and free of the flush-ordering
tradeoff the two above carry: a kernel line is *inserted* into the byte
stream, and `is_kernel_line` already identifies it, so `Serial` can splice the
fragments either side of one back together and match the needle against the
stream userland actually wrote. It repairs the gate, not the log — a human
grepping the T14's log still sees the split line, which is what the two
guest-side fixes are for.

Do not instead shorten needles to fit inside a fragment. The splice point
moves, and a needle short enough to survive every splice is short enough to
match the wrong line.
