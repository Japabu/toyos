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

**The third candidate does not cover it, and the second occurrence is what
shows that.** 2026-08-08, `wt/toyos-apregs`, the same needle split at the same
point — but the intruder is not a kernel line:

```
soundd: hda codec===READY===
0 vendor=1af4 device=0012, 1 function group(s)
```

`===READY===` is `/bin/test-runner`'s, so `is_kernel_line` does not identify
it and a splice keyed on that predicate reassembles nothing. Worse for the
harness specifically: the ready marker is what `wait_for_ready` breaks on, so
the boot capture *ends* inside soundd's line and the rest of it is not in the
string `must_say` searches at all — no reassembly on the captured text can
recover a needle whose second half was never captured. Whatever the harness
does here has to survive an arbitrary userland writer, which points back at
the two guest-side fixes above rather than at a fourth harness one.

Rate, such as one run can give it: three full suites on one tree in one
session, `hda_tone` green in two and red on this in the third, then green 3 of
3 alone on a quiet host. So it is not the audio path and not load in any way
that a re-run answers — it is which two writers happen to collide.

**`println!` is not the escape, and one collision is systematic rather than
chance.** 2026-08-09, `desktop_audio_client` on CI: `soundd: client ` and
`1 removed` came back either side of the kernel's four `exit:` accounting
lines, on run `31271983043` and again on `31282019974` rep 10 — a measured
**1 run in 10**, which is a rate and not a coincidence, because soundd prints a
client's removal exactly while the kernel prints that client's exit. The test
counted one removal of two and waited out its whole 300 s liveness guard.

Moving such a line to stdout does not fix it: `LineWriter` makes it **two**
syscalls rather than one per fragment — `flush_buf()` for what it had buffered,
then `inner.write(lines)` for the rest
(`library/alloc/src/io/buffered/linewritershim.rs`) — so the splice point moves
and does not go away. What does work is building the line and issuing one
`write_all`, which is what `userland/soundd`'s local `say!` does now. That is
one crate of the 176 `eprintln!` sites in `userland/`, `compositor: ready` and
`terminal: ready` among them.

The general fix belongs in `toyos/`, and the reason it was not put there is
worth knowing: `toyos/src` is one of the four trees in the content-addressed
toolchain witness (`specs/testing-strategy.md` §9), so touching it rebuilds the sysroot
and `--pr` refuses a branch that mixes it with other work.
