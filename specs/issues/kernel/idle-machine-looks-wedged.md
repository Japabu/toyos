---
status: open
kind: finding
opened: 2026-08-08
task: 156
---

# Seven T14 boots in seven minutes, and the signature everyone has read as a wedge is what a healthy idle machine writes

**Superseded by #156, which found the defect** — cpu0 arming the LAPIC one-shot
for less time than it takes to arm it, and the Ring 0 stub replaying that
forever. Kept because
its elimination of the log-shape argument is what made the heartbeat build worth
flashing, and because it is the record of three rounds that read the ending of a
quiescent machine's log as a wedge.

**The evidence is committed**: `specs/metal-logs/2026-08-07-freeze/`, seven
consecutive boots of one image off the owner's stick, 22:26–22:33 on
2026-08-07, five of them frozen with Ctrl+Alt+D producing nothing on every one.
Its `README.md` has the table. This entry is what the seven establish, what they
eliminate, and what they cannot reach.

**Nothing in a frozen boot's log distinguishes it from a healthy one.** Diffed
with timestamps, addresses, CPU ids and TSC jitter normalised, a frozen boot and
the healthy one are identical up to the moment the frozen one stops — the only
differences are SMP join ordering and where one `shm: mapped WriteCombining`
line falls. Four of the five end at the same line, `spawn: /bin/filepicker
pid=3`, between 0.945 s and 1.462 s.

**That ending is not evidence of a wedge, and reading it as one is what has cost
this investigation its last three rounds.** It is what a healthy, fully
quiescent T14 writes, and three separate facts force it:

1. the log ring's only drains are the idle loop and the timer tick;
2. a CPU whose pass finds no work and no deadline stops its LAPIC timer and
   halts (`TimerPlan::Stop`), so on a machine with nothing to run there is no
   idle loop and no tick either;
3. the pre-halt check in `sched::driver::execute` refuses to sleep while
   `log_ring::file_has_pending()`, so the last CPU to go down flushes everything
   first. **The file is therefore complete as of the moment the machine went
   quiet, and silent about everything after it.**

In the healthy boot `223244` the very next line after `spawn: /bin/filepicker`
is the owner's first keystroke, 445 ms later. In `223152` it is 1.958 s later.
The log has no other reason to say anything.

**One of the five freezes is closed outright, and it is the control the other
three needed.** In `222741` the firmware left the controller at `cfg=0x30` —
translate **off**, where every other boot in the set reads `0x77` — the set
query answered `0xEE` as it always does on this EC, and `init` took its
fail-closed branch:

```
i8042: ok selftest=0x55 cfg=0x30->0x60 port1=ok port2=ok
i8042: kbd DISABLED - the set query answered 0xee and firmware's cfg 0x30 has
       translate off, so nothing says what the wire carries
```

It returns before the aux port, so that boot had **no keyboard and no
TrackPoint**, by the driver's own correct decision. Ctrl+Alt+D could not have
worked on it whatever the scheduler was doing. And its file is the same shape as
the other three — same ending, and earlier only because the refusal returns
before the keyboard and aux stages: `Boot: peripherals ready (448ms)` against
`(841ms)` on every other boot in the set. A boot with provably no input path is
indistinguishable from the ones being investigated.

**`223152` is the fifth, and it is the sharpest.** Input worked exactly once —
`the pin asserts — 1 interrupts, 1 bytes, 1 keys` at 3.397 s — the filepicker
acted on it, `/bin/terminal` and `/bin/shell` spawned at 3.494 and 3.539, and
then nothing ever again. That is the same shape as "the T14 lost every
integrated input at 6.6 s" in `specs/issues/hardware/`, on a different boot and a different image.

**Eliminated, by reading and not by a run.** `drain_serial`'s `BackendGuard::lock`
was the named suspect and it cannot be this. `SERIAL_SINK` is false for the
whole boot on a machine with no 16550 and no virtio-console, so `append` never
advances `LogRing::len`, `OWED` stays 0, `drain_into` returns 0 on the first
call and `drain_serial` leaves its loop having held the guard across one memcpy
of nothing. `specs/issues/hardware/` reached the same conclusion by the same route in a different
week; it is restated here because the hypothesis keeps coming back.

**What is left, and none of it is decidable from these logs.** Three hypotheses
that all produce exactly the file above:

- **A.** The machine is alive and quiescent, and the i8042 has stopped
  delivering. Three sub-causes, opposite in where the fault lies: the controller
  holding a byte no ISR will read (delivery is edge-triggered, so it never
  asserts again), a redirection entry that got masked or re-pointed, or an EC
  that simply stopped sending.
- **B.** CPU 0 is deaf — spinning with `IF` clear, or wedged — and the machine is
  otherwise alive. **On this kernel that is indistinguishable from a total
  freeze from outside**, because `pci::MSG_ADDR` targets APIC 0 for every
  device's MSI and `init` routes GSI 1 and GSI 12 to APIC 0 as well: every
  interrupt source the machine has lands on one CPU.
- **C.** The machine stopped.

#### What to do at the machine, in order

**Zeroth, and it costs a glance: what is on the panel?** Nobody has recorded it,
and the compositor's own source makes it a real split rather than a curiosity.
`Session::new` ends:

```rust
let mut damage = Damage::default();
damage.add(desk.screen);
eprintln!("compositor: ready");
Command::new("/bin/filepicker").spawn().ok();     // <- the last line in four of the five logs
```

Nothing has been painted at that point: the wallpaper was rescaled into RAM, the
whole screen was *staged* as damage, and the first composite happens in the
caller's loop, **strictly after the `spawn:` line the frozen logs end on**. The
panel therefore answers a question the log cannot:

- **wallpaper** (with or without the filepicker's window) → the compositor
  returned from that syscall and completed its first frame. The machine got past
  the last line in its own log, and the three hypotheses below are the live ones.
- **the kernel's boot log, 8x16, still from `boot_checkpoint`** → it did not.
  `screen_claimed_by_userland` fires at the *claim*, several lines earlier, so
  the checkpoint had already stopped repainting and what is on the glass is the
  last thing the kernel put there. That puts the failure between the compositor's
  `spawn()` and its first blit — one syscall wide, on the process that had been
  running fine a millisecond earlier — and it is #142's shape, not a scheduler
  that stopped.

`223152` is the one boot where this is already known: it ends at `spawn:
/bin/shell pid=5`, two composites and one keystroke past that boundary.

**First, and it needs no reflash: plug a USB keyboard into the frozen T14.**
This is the input-independent source the dump has always needed and it already
exists — `keyboard::handle_key` is the single production path for every keyboard
in the machine, the Ctrl+Alt+D hook is inside it, and `xhci::hid` reaches it
through `keyboard::handle_report`. A hotplug raises a Port Status Change Event,
whose MSI wakes a halted CPU, and `poll_if_pending` enumerates from `drain_irqs`.
So:

- `xHCI: port N connected` appears in `/log` and the keyboard types → **A**, and
  the machine was alive the whole time.
- Ctrl+Alt+D on it paints the dump → **A**, with the full scheduler state as a
  bonus.
- Nothing at all → **B or C**, which the build below then separates.

**Then the one reflash: `cargo run -- --build-only --kernel-feature heartbeat`,
and flash `target/bootable.img`.** The *ordinary* image and not `--diag-boot`:
the freeze happens with the desktop up, and the diagnostic image has no
compositor, so it is a different workload and would not be a re-run of these
seven. *The heartbeat was never compiled into the image that produced them* — no
`alive=` line appears in any of the seven — and it is the instrument built for
exactly this question. It brings
`diag-tick`, so no CPU sleeps longer than 100 ms and the idle loop keeps running
on a machine that has gone quiet. Reading it:

| what the log does | which hypothesis |
|---|---|
| `alive=8/8 … ran=0` continues through the freeze | **A** — and the `i8042: line` beside it says which sub-cause |
| `alive=7/8 mask=0xfe` and `heartbeat: cpu0 last reached one N.NNNs ago` | **B**, dated |
| heartbeats stop at T | **C**, dated |

The `i8042: line` beside every heartbeat is new (this task) and is what makes
**A** actionable rather than merely named: `status` with bit 0 set on sample
after sample is the controller holding a byte, bit 16 of an `rte=` is the mask,
and a clean reading with `irqs=` flat puts the fault at the EC and takes this
driver out of it. `kernel_heartbeat` gates it against the vector `init` says it
programmed.

**Two things about that build, said out loud.** It is an *active* instrument:
`diag-tick` holds the machine out of full quiescence, so a freeze that needs
deep idle may not happen at all under it — which is itself a finding, and worth
recording rather than re-running away. And four heartbeats a second each carry a
`sync_mount` of the stick, so it is a diagnostic budget and not a shipping one.

#### What was deliberately not built

**A heartbeat that summons the dump by itself** when a CPU has been missing from
the mask for several periods. It is the obvious next step for **B** — it would
turn `cpu0 last reached one 4.2s ago` into a symbolised `rip` through
`dump::probe_silent`'s NMI, with no keystroke needed — and `dump::request` is
reachable from the idle loop as written (preempt count 0, holds nothing). It was
not built because it needs an actuator of its own to be gated: `dump-deaf-cpu`
stages a 400 ms window and calls `request()` itself, so it can neither reach a
multi-period threshold nor let a test attribute the dump. **The owner reflashes
once**, and an ungated path in that image is worth less than the resolution it
would add. Whoever picks it up should build the actuator first.
