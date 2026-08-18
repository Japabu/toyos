# Eight consecutive T14 boots, 2026-08-08 11:56–12:03 — the boots that named #156

Off the owner's ThinkPad T14 Gen 2's `TOYOS-LOG` partition, one file per boot,
copied byte for byte, plus a photograph of the panel for each. The image was
built with `--kernel-feature heartbeat`, so every boot carries a line four times
a second saying which CPUs are still reaching a scheduler pass; Ctrl+Alt+D was
pressed on seven of the eight and produced a readable blocked-task dump on every
one of those.

Committed for the reason `2026-08-07-freeze/` was: the analysis cites specific
lines of specific files, and a scratchpad path does not survive the session. The
photographs are here because they are the **only** record of the NMI-answered
`rip`, which the log cannot carry — the CPU that would have written it is the one
that stopped. They are 12 MB, which the dependency audit's binary-file ledger
should count (`issues/build/`).

## What the eight say, in one line

**cpu0 stops reaching a scheduler pass, in every boot, and never comes back**;
seven CPUs stay alive and dispatch nothing at all; and the NMI finds cpu0 in the
LAPIC timer path every time it is asked.

| file | photo | cpu0's last pass | mask after | dump verdict |
|---|---|---|---|---|
| `2026-08-08-115617.log` | `boot1ctrlaltd.jpeg` | 1.509 s | `0xfe` | `arm_one_shot+0x8d` |
| `2026-08-08-115753.log` | — (boot 2, owner: "same") | 1.756 s | `0xfe` | not photographed |
| `2026-08-08-115842.log` | `boot3ctrlaltd.jpeg` | 1.764 s | `0xfe` | `arm_one_shot+0x143` |
| `2026-08-08-115919.log` | `boot4.jpeg`, `boot4ctrlaltd.jpeg` | 1.504 s | `0xfe` | `timer_entry+0x195` |
| `2026-08-08-120017.log` | `boot5ctrlaltd.jpeg` | 8.743 s (cpu1: 8.798 s) | `0xfc` | `timer_entry+0x0` on **both** |
| `2026-08-08-120121.log` | `boot6ctrlaltd.jpeg` | 1.517 s | `0xfe` | `arm_one_shot+0x103` |
| `2026-08-08-120157.log` | `boot7ctrlaltd.jpeg` | 4.619 s | `0xfe` | `timer_entry+0x0` |
| `2026-08-08-120249.log` | `boot8ctrlaltd.jpeg` | 2.815 s | `0xfe` | `timer_entry+0x195` |

The owner's own account of the session, verbatim:

> boot 1: froze immediately in compositor. ctrl alt d works. boot 2: same, 3:
> same, 4: stuck before compositor, 5: responsive in compositor stuck after
> tone, no audio, ctrl alt d works, 6: froze before compositor, 7: responsive
> compositor froze after i entered tone, no audio, ctrl alt d works, 8: froze at
> compositor idling. the freezes feel like theyre not related to tone.

He is right that it is not tone, and the logs are stronger than his impression:
**only boot 5 ever started `/bin/tone` at all.** Boot 7's `tone` was typed at a
shell that was already gone — cpu0 stopped at 4.619 s and the shell had been
placed on it, so the word never reached a spawn and the log has no `/bin/tone`
line. The other six froze with nothing audio-related in the picture.

Boot 5 is the one that says which client asked. cpu0 stopped at 8.743 s and cpu1
at 8.798 s, and `soundd: resumed` is the line before: the mix loop had just
started asking for a wake at the next period grid point, which it spells
`.max(1)` — one nanosecond — whenever that point is already past
(`userland/soundd/src/main.rs:955`, `:1327`). Five of the other seven stopped
between 1.504 s and 1.764 s, inside the compositor's first frames — and the
compositor's drain loop spells the same thing `Duration::from_nanos(1)`
(`userland/compositor/src/session.rs`). The remaining two stopped at 2.815 s and
4.619 s with the desktop idling, which is that loop still running.

## The eight NMI samples

Every dump's `cpu0 NMI answered, it is here:` line, transcribed from the
photographs. The kernel base is the same in all eight (same image, and this
firmware places it identically), so the absolute addresses are directly
comparable: `arm_one_shot` begins at `0xffff80005bee6c10` and `timer_entry` at
`0xffff80005be35b78`.

| boot | rip | symbol |
|---|---|---|
| 1 | `0xffff80005bee6c9d` | `kernel::arch::apic::arm_one_shot+0x8d` |
| 3 | `0xffff80005bee6d53` | `kernel::arch::apic::arm_one_shot+0x143` |
| 4 | `0xffff80005be35d0d` | `kernel::arch::idt::timer::timer_entry+0x195` |
| 5 | `0xffff80005be35b78` | `kernel::arch::idt::timer::timer_entry+0x0` (cpu0 **and** cpu1) |
| 6 | `0xffff80005bee6d13` | `kernel::arch::apic::arm_one_shot+0x103` |
| 7 | `0xffff80005be35b78` | `kernel::arch::idt::timer::timer_entry+0x0` |
| 8 | `0xffff80005be35d0d` | `kernel::arch::idt::timer::timer_entry+0x195` |

**Two of those offsets are exactly the instruction after a `wrmsr` to MSR
`0x838`, which is `X2APIC_TIMER_INIT`.**

- `timer_entry+0x195` is `mov byte ptr gs:0xf4, 1` — the `need_resched` store
  that follows the Ring 0 stub's reload of the one-shot. `timer_entry` is a
  naked function whose asm has not changed since 2026-07-31, so this offset is
  build-independent and was read straight out of `llvm-objdump`.
- `arm_one_shot+0x8d` is `mov edi, gs:0x88` — the first instruction of the
  trace record that follows `arm_one_shot`'s own `wrmsr` of the same register.
  `+0x103` and `+0x143` are two more instruction boundaries in that same tail,
  a few tens of instructions further on and still before the function returns.
- `timer_entry+0x0` is the first byte of the entry stub: an NMI that arrived
  while the timer interrupt was being delivered is recognised at the handler's
  first instruction boundary.

So all eight samples are inside one loop — arm the one-shot, take it, reload it,
return, take it again — and cpu0 travels none of the few thousand instructions
between there and the next `drain_irqs` in as long as 18 seconds.

## The three numbers the mechanism rests on, all from these logs

- `LAPIC timer: 384007 ticks/10ms` — the APIC timer runs at 38.4 MHz, so **one
  tick is 26 ns**, and 26 ns is what `ticks.clamp(1, …)` armed for any deadline
  that was already past.
- `TSC: 2419MHz` — 26 ns is **63 core cycles**. A single `wrmsr` does not retire
  in 63 cycles, so the interrupt is already pending when the instruction that
  armed it completes, and the `iretq` out of the handler has no shadow.
- `heartbeat: … ran=0` on every line after cpu0 goes quiet — seven live CPUs,
  not one task dispatched, machine-wide. The dumps say why: `== sched: 7/8
  cpu(s) answered … 0 running, 0 queued, 5 parked` with `ready: pid=0 tid=0
  compositor` above it. The compositor is ready on cpu0's queue, and a steal is
  answered by the victim inside its own pass — which cpu0 never runs.

## What kept working, and why that is the diagnosis rather than a puzzle

`i8042: line status=0x1c irqs=N bytes=N` keeps climbing after cpu0 dies —
115617 goes 288 → 304 across the last 1.3 s — and GSIs 1 and 12 both route to
APIC 0. So **cpu0 was still taking and servicing interrupts the whole time.**
That is what a timer livelock looks like: the i8042's vector `0x24` outranks the
timer's `0x20`, so it is delivered first at each `iretq` boundary, and the NMI is
maskable by nothing at all. What starves is only the interrupted program.

## What the eight do not establish

They do not measure the interval that was armed. Four distinct offsets inside
one function say cpu0 retired *some* instructions rather than being pinned at a
single byte, so the count was of the same order as the interrupt round trip
rather than provably the one-tick minimum. Nothing here distinguishes those, and
the fix does not depend on which it was: any count that does not outlast the
interrupt it schedules produces this, and the Ring 0 stub replays it verbatim.

`boot4.jpeg` is the panel rather than a dump — boot 4 froze before the desktop
painted, and the photograph is what the owner saw before he pressed Ctrl+Alt+D.
`boot4ctrlaltd.jpeg` is the dump that followed.
