---
status: assigned
kind: defect
opened: 2026-08-05
task: 142
---

# A spawned process sometimes never starts, and every stuck terminal in the T14 session is downstream of one

Owner-reported (task #141) and read off two T14 session logs. **It is one
defect, not the two it looked like from the symptoms**, plus a second
independent one in a fork (`winit-app-never-exits`). The investigation is the
scheduler agent's; what is here is the evidence and the eliminations, so it is
not re-derived.

**What the log establishes.** `/bin/ls` was spawned twelve times. Ten exited
`code=0` in 12–59 ms. Two — pid 10 at 692.459 s and pid 26 at 904.327 s —
**produced no output at all and never exited**, and neither did `/bin/rustc`
pid 18 at 826.991 s. Nothing distinguishes their `spawn:` lines from the
healthy ones: same binary, same `ELF: 3740 relocations indexed`, same
`layout=0ms relocs=0ms deps=0ms tls=1ms total=1ms`. The kernel-side spawn
finished and said so; the process then did nothing, forever.

**Every other symptom is a consequence of that one.** A shell blocked in
`waitpid` on a child that never dies is behaving correctly, and so is a
terminal blocked in `child.wait()` on that shell
(`userland/terminal/src/main.rs:186`). Pairing them off:

| terminal | shell | child it is waiting on |
|---|---|---|
| 5 | 6 | `ls` 10, hung |
| 11 | 12 | `rustc` 18, hung |
| 19 | 20 | `snake` 23, see `winit-app-never-exits` |
| 24 | 25 | `ls` 26, hung |
| 27 | 28 | none — **and it is the only pair that exited** |

Shell 28 is the healthy control the same log offers: 95 syscalls, `spawn 3`
paired with `waitpid 3`, and it went on from `ls /bin` to `free` and then
`doom`. So neither a lost exit notification nor a missed wakeup is involved,
and `sys_waitpid` registering on the park lot before it reads the table
(`kernel/src/arch/syscall.rs:1067`) is doing its job.

**Refuted, and it was the first hypothesis because it was a same-day change**
(#129, `85a8433`): a child that takes a surface grab and dies without releasing
it does *not* leave the terminal mute. `surface::Host::poll` clears the grab on
`RxStep::Eof` (`toyos/src/surface.rs:218-224`, `close` at `:322`), the terminal
polls every client fd every pass (`userland/terminal/src/main.rs:73-75`,
`:107`), and **the only program in the tree that grabs is `locale detect`**
(`userland/toybox/src/locale.rs:161`) — neither `ls` nor `snake` does.

**Not reproduced, and here is exactly what was tried** so nobody repeats it. A
guest binary modelled on the chain — a parent owning `tty_piped` stdio and
draining it, a shell role whose stdio is those pipes, spawning `/bin/ls /bin`
with `Stdio::inherit()` and waiting with a 2 s per-child ceiling — ran **120
children in the shared boot (smp=2) and 120 more on a dedicated smp=8 boot,
and every one of them started and exited.** The T14 has eight CPUs, so the
CPU count was the first fidelity gap closed and it was not enough. The chain
*under a live compositor and a real `/bin/terminal`* was the next fidelity step
and was **not** taken: `tests/metalcase`'s initrd carries no terminal, shell or
toybox, and five other tests share that boot.

**The measurement meant to decide it was `ps` — and `ps` is a victim.** The
plan was to read the hung child's state column (`userland/toybox/src/ps.rs:72`)
and split three ways: `R` with no CPU is a task nothing ever picked up, `S`
with no CPU is one that blocked before its first user instruction, and any CPU
at all moves the fault into userland startup. The owner ran it on the T14
during boot 10 of `boot5-doom-wedge.log` and **`ps` pid 17 printed nothing and
never exited**, and neither did `/bin/shutdown` pid 25 or three `doom`s. In
that whole boot the only processes that ever exited were `netd` and one
`locale`.

That is an answer rather than a lost measurement, and it is worth more than the
column would have been: **it strikes a fresh process before it can write a
byte, whatever the program is** — `ls`, `rustc`, `ps`, `shutdown`, `doom` — so
nothing about `ls`'s own I/O path is involved. The kernel logs to the end.

The per-CPU `parked` counters climbing 1 → 2 → 3 is **not** the victims
accumulating, and `ready = 0` on every report is not evidence of anything: a
thread that never ran cannot be in `parked`, and the line is printed from the
idle loop. Both numbers are fixed by where they are printed. `spawn-placement-unlogged`
carries that elimination, and the one instrument that would settle the split
the `ps` column was going to.

Investigation is the scheduler agent's (#142); the shapes are consistent with
one defect. Ctrl+Alt+D is now machine-wide and process-named (`issues/diagnostics/`), and on the
owner's laptop one press named three CPUs not reaching a scheduler pass and
three threads ready-and-never-run.
