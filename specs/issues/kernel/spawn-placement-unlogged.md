---
status: open
kind: finding
opened: 2026-08-05
---

# A spawned thread that never runs is invisible: `spawn:` does not record where it was placed

From the T14 field log `boot5-doom-wedge.log` boot 10 (17:41, pre-fix image).
Spawned processes intermittently never execute a first instruction, worsening
over the session: `/bin/ps` pid=17 (69.4 s), `/bin/doom` pid=20 (79.8 s, not
even its banner), `/bin/shutdown` pid=25 (104.7 s). Two earlier dooms
initialized fully, drew their title screen and went silent at exactly `ST_Init`
— where doom starts its further threads — with soundd printing `opening stream`
for pid=11 and never `client 1 connected`. The kernel stayed alive to 114.6 s.

**The `sched:` counters do not indict the scheduler, and cannot.** Two facts
about where that line comes from:

* `parked` is `cpu.parked().count()`, and the only way into `CpuSched.parked`
  is `SchedPass::dispose_block`, which consumes `cpu.running`. **A thread that
  never executed cannot be counted there.** The victims are not in that number.
  What it does count is ordinary long-lived blocked threads — compositor,
  soundd, filepicker, and one per live terminal/shell — so 1 → 2 → 3 as three
  terminal/shell pairs accumulate is the expected reading, not a leak.
* `ready=0 current=None` on every sample is a tautology of the print site:
  `log_health` is called from `idle_loop`, so the CPU printing it is idle by
  construction. CLAUDE.md already says this line is not a heartbeat.

**What the log does isolate is `spawn`.** `driver::spawn` posts its `Msg::Adopt`
with `Urgency::Normal` to `placement()`, and `placement()` picks the CPU with
the lowest *published* load — which on a mostly-idle machine is a **halted**
one. So a spawn is the most delivery-dependent operation in the system: unlike a
wake, which goes to a task's home CPU where other work usually is, a spawn
routinely aims its only reap-or-run event at a CPU that must be interrupted to
see it, and then must complete a whole pass before the thread's first
instruction runs. Everything in `scheduler-pass-blocks-in-xhci` — `drain_irqs`'s xHCI recovery
ahead of the mailbox drain, `log_file::poll()` ahead of the idle loop's `pass()`
— sits between that IPI and that first instruction. This machine boots off the
stick it logs to (`usb-storage: disk 0 ready on slot 3, SanDisk Ultra`, and
`9266 bytes of kernel log still on the stick` at the end), so both of those are
USB transfers on `USB_TIMEOUT_NS`.

**It is not the balance-path defect (#142) wearing userland clothes.**
`hand_off`'s kill check fires only for a task whose retire is already claimed; a
freshly spawned thread has no kill bit, so that fix cannot reach this. What the
two share is the *state*: `InTransit` is reachable only by the adopt that
carries it, #142 removed one producer of stuck ones, and `spawn` is the other
producer and is untouched.

**The instrument that is missing.** `loader.rs:1111`'s `spawn:` line records
pid, tid, base, entry, cr3 and five timings, and **not the CPU the task was
placed on**. So no reader of this log can say which CPU owed pid=17 its first
dispatch, and therefore cannot correlate a never-ran spawn against that CPU's
`sched:` lines — which is the one correlation that would separate "the adopt was
never delivered" from "the destination never completed a pass". Fold `dst` into
that line rather than adding a second one; the log ring is itself one of the
conditions that keeps a CPU awake.
