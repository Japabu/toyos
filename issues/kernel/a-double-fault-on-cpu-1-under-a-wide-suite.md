---
status: open
kind: finding
opened: 2026-08-18
---

# A `DOUBLE FAULT` on CPU 1 killed a guest under a twelve-wide suite, and nothing kept its report

Seen once, dev host, `cargo test` twelve wide, 2026-08-18, on
`wt/toyos-purecrates` — a branch whose whole delta is three kernel files moving
into two pure crates with **no line of their logic changed** (`diff` of each
moved file against its original: doc comments, one `use` path and two test
paths, nothing else). The same tree is green on all twelve KVM shards of CI
(PR #124).

```
FAIL console_line_atomicity: kernel panic: DOUBLE FAULT on CPU 1 (pid=Some(Pid(2)) tid=Some(Tid(0))) — the guest went quiet because every CPU is halted, not because it was still working. The panic is the finding and the guard never got to be one.
stdout:
logd: this boot's kernel log is /log/2026-08-18-113110.log (2026-08-18 11:31:10 at UTC+0 recovered f
soundd: suspended
  FAIL  console_line_atomicity  (21s)
```

`ALONE console_line_atomicity: GREEN`, by the harness's own re-run. The host
line for that run: `fastest boot 1522 ms against the reference 1320 ms —
liveness ceilings paid at 1.15x width`. The same name passed in 9 s in the full
run before it and red in the full run after it on a different and already-filed
defect (`writer A declared 1000 whole lines and the capture carries 798`,
`issues/build/console-line-atomicity-reds-on-a-short-capture.md`), so
what this name does under load is red in more than one way.

**This is the sighting
`issues/build/every-recorded-stall-predates-the-panic-discriminator.md`
said would start arriving.** That file's closing paragraph names the dev-host
load family and says `ALONE: GREEN` is not evidence against a panic — a panic
reached only under contention does not reproduce alone either. Before
2026-08-17 this run would have been recorded as a stall.

## The diagnostic half is fixed

The kernel's own report was never printed: `console_line_atomicity`'s failure
arm returned `format!("{err}\nstdout:\n{}", tail(&result.stdout))`, and `stdout`
is the *userland* half of a test's window, so two daemon lines were everything
that survived. It was not one arm. Counted on the tree, 2026-08-18: **52 sites consume
`TestResult::error` — 50 of the shape `if let Some(err) = &…error`, plus
`metal_sim_window_drag`'s `error.is_some()` arm and the shared-boot runner's
`reason`. Sixteen of the 50 print `serial` (or its kernel lines, or its tail)
in the arm's own message; 34 do not, and neither of the other two did — 36
verdicts in all that dropped the guest's own account.** `metal_sim_window_drag`
was worse than dropping it: it printed the verdict through `{:?}`, which renders
a multi-line report as one line of escapes.

Fixed at the field rather than at the arms: `TestResult::error` is a
`qemu::WaitVerdict`, whose one constructor takes the capture the verdict was
reached on as its second argument, and which appends
`serial::death_report` — everything from the line the kernel announced its own
death, bounded at eighty lines and saying so when it truncates — whenever that
capture carries one. A wait that reports a kernel death without the text
explaining it is no longer expressible, and every arm that formats the field
gets the report whether or not anyone was ever going to edit it. `before` and
`serial` are handed over in that order, so a death *before* the test announced
itself is recovered too (`issues/build/a-failure-message-drops-the-lines-before-the-test-started.md`
is that hole's general form and stays open — this closes it for a kernel death
only). Gated in both directions under `serial_vocabulary`, and against the real
`#DF` `double_fault_stack` already puts on the wire.

**So what the next sighting shows that this one did not** — every line of it,
verbatim, in the failure message:

- `cr2=… (address that caused the fault chain)` — the address the chain started
  on, which is the single most diagnostic word there is and the one that was
  lost;
- `rip=… rsp=… rbp=…` off the `#DF` frame;
- `debug_page_walk(cr2)` — PML4E, PDPTE, PDE, PTE, with `PS=` and the
  present bits, which says whether the address was a guard page, an unmapped
  hole or a mapped page refused for another reason;
- `Kernel backtrace:` with the `rip` resolved against the kernel's symbols;
- the 4 KiB scan up from `frame.rsp` for the **original** exception frame, and
  when it finds one: `rip`, `cs`, `rflags`, `rsp`, `error_code`, and then either
  the user context and a user backtrace or `Original fault was in kernel code`
  and a second kernel backtrace from the original `rbp`;
- and from `halt_all_cpus`, after `panic_flush`: `[ist1] used N of M bytes,
  guard intact` — whether the report overflowed the stack it was written from.

What the handler still cannot say, and what no fix to the harness reaches: **the
CPU records neither of the two vectors that combined.** `#DF` pushes an error
code that is architecturally zero. The scan for the original frame is the only
evidence of what the first fault was, and if `frame.rsp` is itself unreadable,
`safe_read_kernel` fails on its first word and the scan prints nothing — which
is itself the tell for a stack that had gone.

So one step is still worth taking by hand at the next sighting, because no fix
to a failure arm reaches it: boot it with `BootOptions { qmp: true, .. }` so the
guest survives the verdict, and ask `human-monitor-command` for
`info registers -a` over the lane's socket to see what the *other* CPU was
doing. Take that capture before injecting anything — a keystroke revives a
halted CPU, so Ctrl+Alt+D both confirms the diagnosis and destroys the evidence
for it.

## What the surviving line establishes about the machine

**`pid=Some(Pid(2)) tid=Some(Tid(0))` is `/bin/logd`'s main thread.** Measured,
not inferred — the boot console of a `console_line_atomicity` run on this tree,
same config (`tests/testcases`) and same width (`smp: 2`):

```
[kernel 0.233 cpu0] spawn: /bin/init pid=0 tid=0 …
[kernel 0.235 cpu0] kthread: klogd pid=1 tid=0 runs in the kernel address space; a panic in it halts the machine
[kernel 0.260 cpu0] spawn: /bin/logd pid=2 tid=0 …
[kernel 0.291 cpu0] spawn: /bin/soundd pid=3 tid=0 …
[kernel 0.297 cpu0] spawn: /bin/test-runner pid=4 tid=0 …
[kernel 0.312 cpu0] spawn: /bin/test_rs_console_line_atomicity pid=5 …
```

Pids come from `IdMap`, which starts at zero and never reuses; pid 1 is the
`klogd` kernel thread, which is why logd is 2 rather than 1.

**That pid excludes the idle stack, which is the only stack in this kernel whose
overflow the CPU can turn into a `#DF`.** `percpu::current_pid`/`current_tid`
are written on every context switch from the incoming task's id
(`kernel/src/hw.rs`), and the idle context's id is `None`
(`sched/driver::enter_idle_loop` sets both to `None` before it moves `rsp`). A
live pid therefore means CPU 1 was running a task. The idle stack is the one
with an unmapped page under it (`IDLE_GUARD_SIZE`, `guard_kernel_page`), and an
unmapped page under a stack is what turns an overflow into a `#PF` the CPU
cannot deliver — because the frame it must push goes to the same bad `rsp`.

**And the stack it *was* on does not fault on overflow.** A thread's kernel
stack is `KERNEL_STACK_SIZE` = 128 KiB of ordinary heap (`OwnedAlloc::new` in
`loader::alloc_kernel_stack`) with a canary word at the bottom
(`write_stack_canary`) checked at every scheduler pass (`sched::driver::check_stack_canary`)
and **no unmapped page beneath it**. An overflow off it writes into whatever the
allocator put there and is reported later, from ordinary context, as
`KERNEL STACK OVERFLOW: tid=N` — never as a `#DF`.

So the textbook first reading of a double fault — a kernel stack overflow — is
the one this kernel's shape argues *against*, on the strength of the one line
that survived. Whoever reads the next sighting should say so out loud before
reaching for it.

IST1 cannot be the cause either: `ist 1` is on the `DoubleFault` row alone
(`arch/idt/mod.rs`), so nothing but a `#DF` ever runs on it. It can only be
overrun *while reporting* one, and the report says whether it was —
`[ist1] used N of M bytes, guard intact` against the `IST1_STACK_SIZE` of 16384
that `0b4f305` and `9148057` sized, whose declared margin is half the stack.
Measured on this tree on 2026-08-18, two `double_fault_stack` runs minutes
apart: **6688** and **5312** bytes used of 16384, guard intact both times. The
1376-byte spread between two runs of one staged fault is worth knowing before
anybody reads a single number off a sighting as *the* cost of the report.

## The class this leaves, and the one live instance of it

The only `#DF` this project has ever diagnosed had exactly this signature and
was neither textbook reading. `9bd7a9e`, 2026-08-08:
`DOUBLE FAULT on CPU 0 (pid=Some(Pid(3)))`, wide and alone, and the cause was
**a fault delivered through an IDT gate with `P=0`**, which the CPU takes as a
second contributory fault. So the class prior for "a `#DF` naming a live pid" in
this kernel is a missing gate, not a stack.

What survives of that class was audited here, in code:

- the legacy 8259 is remapped off the exception range and then masked outright
  at `idt::init` (`disable_pic`, `0xFF` to both OCW1s), and `ioapic::init` masks
  every redirection entry firmware left, so neither can deliver a vector at all.
  **Closed.**
- every vector Intel names for 64-bit mode has a gate. **Closed by `9bd7a9e`.**
- **the LAPIC's spurious-interrupt vector is `0xFF` and the IDT has no entry at
  `0xFF`.** That is a defect on its own terms and is filed as
  `issues/kernel/the-lapic-spurious-vector-has-no-gate.md`. **It is not
  claimed as the cause of this sighting** — a spurious interrupt leaves nothing
  behind, and what would deliver one on *this* configuration is itself
  unestablished (that file has the reading). It is recorded here because
  a `#DF` naming a live pid is precisely what it would look like, and because
  closing it removes one of the three readings the next sighting has to be
  weighed against.

## Reproduction

**Not reproduced.** Three full `cargo test` suites on this tree, 2026-08-18:
265 of 265 at `1.05x width`, 265 of 265 at `1.36x`, and 264 of 265 at `1.12x`
whose one red was `sched_check_build`'s pass-cost distribution — `KNOWN-RED`,
`FIRES 6 of 10` on the dev-host-loaded instrument, and no `DOUBLE FAULT`
anywhere in that run. One of the three was above the `1.15x` the sighting was
taken at and it was 265 of 265 green; the other two were below it. That is three
samples against one observation and it establishes no rate whatsoever; it is
recorded so nobody counts it as evidence in either direction.

Nothing here is a repro recipe, and there is no cheap one to offer: a `#DF`
reached only beside eleven other guests does not reproduce alone, and
`issues/kernel/ring0-jump-to-zero-under-port-polls.md` says the same of its own.

**One recipe was cheap for the neighbouring class and nobody had tried it**,
which is worth a sentence here because the move transfers: booting the ordinary
`target/bootable.img` twelve at a time in a loop, five seconds a boot, killing
each guest and grepping its console. That reproduced the Ring 0 fetch at `0x1b`
at roughly one boot in a hundred and diagnosed it. A machine-wide death *during
boot* does not need a suite — it needs boots, and a suite is an expensive way to
get 79 of them.

Do not close this on green runs. What retires it is a second sighting that names
its own cause — which it now will.
