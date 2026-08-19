---
status: open
kind: track
opened: 2026-08-10
---

# The BOT/SCSI machine is still hand-written in the kernel

The xHCI port, protocol, teardown, recovery and enumeration machines are pure
crate code with a host simulator behind them, and the kernel drives them. The
mass-storage half is not: the BOT round trip and the SCSI bring-up above it are
still hand-written in the kernel's wait module, and that is the one call site
where a scheduling pass can still spend its transfer budget inside xHCI — for a
disk arriving *after* boot, which is one greppable path.

**What to build**, expressed the way recovery and enumeration already are: the
round trip (command block out, data, status in, one legal stall retry) and the
bring-up above it (test-unit-ready on a budget, sense, inquiry, read-capacity 10
then 16), with two drivers over the same machine — a blocking one for the
read/write entry points and a stepped one for the bind. Blocked on nothing but
its own size; folding it into the enumeration landing would have made that
unreviewable.

After it, the pass-duration proof costs no new code: one guest gate measuring a
scheduling pass across a plug, plus the existing check-build's pass-cost
distribution. Both premises are now spent.

Constraints the machine has to preserve:

- **One outstanding command, not a queue.** The command ring is one queue and
  the driver is strictly serial; a completion event must be matched against the
  command TRB's *physical address*, or a timed-out command hands its code to
  whoever waits next.
- **A control transfer with a data stage is two completions** — the data stage
  carries ISP and IOC, and the status stage is a second event on the same
  (slot, dci) — so a submission takes a stage count and the answer carries the
  residue as well as the code.
- The lock disables preemption for the guard's whole life, warns at 50M spins
  and panics at 500M, against a 2 s per-command budget. **The ticket lock is
  excluded as the T14 freeze's mechanism** — the owner saw neither the warning
  nor the panic. Do not re-litigate that.
- Rust makes a module's private items visible to its **descendants**, so a
  *view* handed to the poll enforces nothing. The split had to be a module, with
  the poll outside it.
- **There is no gate for the 100 ms unplug window and it cannot be aimed** — a
  QEMU `device_del` cannot land inside it. That is the answer, not an omission.
  Relatedly, QEMU's xHC has no link training and no inactive state: its
  SuperSpeed ports read enabled the moment they are touched, so warm-reset
  correctness lives in the host model only.

`issues/hardware/pulling-the-boot-stick-freezes-the-t14.md` and
`issues/hardware/port-reset-gets-no-second-try.md` are both open and neither is
closed by this.
