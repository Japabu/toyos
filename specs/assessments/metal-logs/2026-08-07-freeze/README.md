# Seven consecutive T14 boots, 2026-08-07 22:26–22:33

Off the owner's ThinkPad T14 Gen 2's `TOYOS-LOG` partition, one file per boot,
copied byte for byte. Five froze. They are the best capture the machine has
given: seven boots of one image inside seven minutes, with the healthy and the
frozen side by side.

Committed because the analysis in `issues/kernel/` cites specific lines of
specific files, and a scratchpad path does not survive the session. Precedent is
`toyos-hda/`, which is host-tested against the committed H0 dumps of both
machines.

| file | outcome | last line |
|---|---|---|
| `2026-08-07-222644.log` | **froze** | `spawn: /bin/filepicker pid=3` at 1.462 s |
| `2026-08-07-222719.log` | **froze** | `spawn: /bin/filepicker pid=3` at 1.393 s |
| `2026-08-07-222741.log` | **froze**, and the keyboard was never armed | `spawn: /bin/filepicker pid=3` at 0.945 s |
| `2026-08-07-222910.log` | ran to 11.2 s, then soundd panicked (unrelated, `§4`) | `exit: soundd pid=1 code=101` |
| `2026-08-07-223119.log` | **froze** | `spawn: /bin/filepicker pid=3` at 1.429 s |
| `2026-08-07-223152.log` | **froze**, after exactly one keystroke got through | `spawn: /bin/shell pid=5` at 3.539 s |
| `2026-08-07-223244.log` | healthy, 15 s, `ps` ran | `compositor: frames=35 …` |

Ctrl+Alt+D was pressed on all five freezes and produced nothing on any of them.

Two things a reader should know before drawing anything from the four that end
at `spawn: /bin/filepicker`:

- **That ending is what a healthy, fully quiescent T14 writes.** The log ring's
  only drains are the idle loop and the timer tick; a CPU with no work and no
  deadline stops its LAPIC timer and halts; and the pre-halt check in
  `sched::driver::execute` will not let the last CPU sleep while the file sink is
  owed bytes. So the end of the file is the moment the machine went quiet and
  says nothing about what happened afterwards. In `223244` the next line after
  `spawn: /bin/filepicker` is the owner's first keystroke, 445 ms later.
- **`222741` is the control.** Its firmware left the controller at `cfg=0x30`
  with translation off, so the driver's fail-closed rule disabled the keyboard by
  name and never reached the aux port either. That boot provably had no input
  path at all — and its file is the same shape as the other three. It reaches
  `spawn: /bin/filepicker` earlier only because the refusal returns before the
  keyboard and aux stages: `Boot: peripherals ready (448ms)` against `(841ms)`
  on every other boot here.

`222910` also carries a soundd panic at 11.2 s
(`repeated completion for free buffer`) which belongs to a different task; do
not read it as part of the freeze.
