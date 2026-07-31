# Metal track review history: sixty-odd defects in trees that were green

Across the 2026-07-30/31 metal-track work — Stage 7c, the §6.4 tripwire, M0 the
panic console, M1 metal-sim, M2 i8042 + I/O APIC, the xHCI DMA rework — every
implementation wave was followed by an adversarial review wave. Each
implementation shipped with its own suite green and its own evidence in the
commit message. The reviews then confirmed **on the order of seventy defects in
code and tests, plus thirty corrections to records**.

The implementers were not careless. Read the commit messages in
`0d2a324..b33b231`: measured A/Bs, in-guest traces, negative-teeth
demonstrations. The defects were invisible *from inside the change* — a claim
about method, recorded here with evidence so it does not become folklore.
Companion to `specs/audio-gate-history.md`, whose lesson (the instrument is wrong
before the subject is) was re-confirmed here about ten times; the forward-looking
half is `specs/device-test-strategy.md`, and this file is its evidence.

## The tally

Counting each item a commit message itself enumerates as one. "Records" are
corrections to specs, CLAUDE.md and code comments that described code never built
or long gone.

| Wave | Commits | Code | Test | Records |
|---|---|---|---|---|
| post-7c | `bcf1fa1` `9cc4e06` `77fc62d` | 2 | 0 | 8 |
| suspend series | `1fa22a8` `01c02e0` `531b0f2` | 3 | 4 | 6 |
| §6.4 tripwire | `72c19bc` `4d86e32` `fd9e133` | 2 | 1 | 4 |
| M0 panic console | `9111396` `ba4d28c` `c92bf13` `938c7ac` | 13 | 5 | 4 |
| M1 metal-sim | `4752f83` `d8e351a` `efbeed7` `1103f6a` `2ab7403` | 1 | 7 | 1 |
| M2 i8042 / IOAPIC | `86c372c` `09f40be` `442f3e8` `2716874` `01f46c5` `789a806` | 19 | 4 | 2 |
| xHCI layout | `5bb673c` `4b3cbd9` `40aff72` `71940c1` `0cdbaba` `71dfe57` | 6 | 3 | 9 |
| | | **46** | **24** | **34** |

`77fc62d` is the informative row: eight findings, **zero** code defects. A review
that finds nothing in the code is not a wasted wave; one that finds nothing at
all has not been run properly.

## Records that lied

`77fc62d`: `specs/scheduler-migration-log.md` recorded that spec §6.4's
preempt-count baseline asserts "landed with the shims at 7a". No such assert
existed anywhere — `driver::pass` only raises and lowers the count, and
`with_cpu`'s nested-pass check is §6.2's `IN_SCHEDULE` replacement, a different
mechanism. Three stages had been signed off against a guarantee that was never
built. It landed for real at `995f3cb`.

This is a repeat. `b3b0126` (2026-07-30, the commit before this arc) retired a
panic-output issue that `e9f3356` had fixed two days earlier, and named the
precedent itself: "the same reading-the-database-not-the-code failure that
previously misreported the PT_TLS overflow as live."

Same class, other direction, same arc: `0cdbaba` found
`specs/boot-image-split.md` still describing `OFF_KB_INT_RING`, a "0xD000-byte
pool" and a panic past three devices — all deleted by `5bb673c` hours earlier —
and still listing removing that panic as work to do. An agent picking up the
image split would have re-derived a fix that had shipped.

## A prerequisite nobody knew existed

The §6.4 task could not start. `946d9f7`: the per-CPU preempt count was never
conserved across a context switch, because contexts owe different numbers of
`enable`s (a task parked in a syscall owes two, one preempted at IRQ exit owes
one) and the count was a per-CPU word handed to whoever landed next. A probe
found, within 0.2 s of boot, `exit_to_user` running at counts 0, 1 and 2 — user
threads returning to Ring 3 with preemption silently disabled. `driver::pass`'s
own comment ("it balances per context, not per call") described an invariant the
code did not have. The depth now lives in `KernelCtx`, swapped by `Hw::switch`.
No test failed before this, and none could: the count's absolute value meant
nothing, so nothing read it.

## Certifications that could not fail

The largest category, and the one that most directly refutes "the suite is
green".

| Certification | Why it could not fail | Fixed |
|---|---|---|
| `capture()` — the panic snapshot | Deleting **both call sites** left all five screen tests green; both fatal tests painted from the live ring instead | `ba4d28c` C1 |
| `screen_recoverable_untouched` | Passed on a *timeout*: `run_test` returns exit code `None` then, and the only premise check was `!= Some(0)` | `ba4d28c` C2 |
| The screen colour assertions | The decoder thresholds `max(r,g,b)`, so alert red and white both decode as foreground; inverting `has_alert` changed no test | `ba4d28c` C3 |
| The two-pass wrap | Every needle fit 256 columns and `screen::self_test` sized its own width to its longest line | `ba4d28c` C4 |
| §6.4 tripwire attribution | `contains("arch/syscall.rs")` over the whole buffer — the same boot's `test_syscall_panic` already printed that path | `4d86e32` |
| `metal_sim_compositor` | Asserted a taskbar row colour and a colour count; a revert of `366bbfe`, the daemon-exit fix it claimed to certify, left it green | `efbeed7` |
| `metal_sim_input`, first two versions | Pixel thresholds after a fixed-coordinate click; passed because the taskbar repaints its clock once a second | `efbeed7` |
| `ioapic_topology` | Asserted `total == hi - lo + 1` against a line printing `hi = gsi_base + entries - 1`: a tautology | `09f40be` |
| `toyos-ps2`'s 10 M-byte fuzz | All four assertions structurally unfalsifiable — `usage != 0` by the emitter, HID range by the tables, `buttons < 8` by a mask, the delta bound by the arithmetic domain | `01f46c5` |
| `xhci_many_devices` + `xhci_slot_exhaustion` | A fixed `MAX_DEVICES = 16` passed **both**: one asserts counts (any cap ≥ 6 gives six devices on six rings), the other matched `contains("device blocks=1")` — a substring of `blocks=16` — on a build that clamps to one regardless | `4b3cbd9` |
| the same test's orphan-slot and ready-marker checks | The first compared two log lines printed from one local; the second ran after `wait_for_ready` had already returned or aborted | `4b3cbd9` |
| `audio_idle_suspend` | Could not tell a suspend from a wedge — both are zero CPU delta. Would have been green on exactly the tree the series existed to fix | `01c02e0` |

The M0 row deserves its own sentence: **five vacuous certifications in one
subsystem**, found by retroactively applying one rule — break what the assertion
guards, watch it go red. `ba4d28c` fixed each paired with the regression it now
catches.

## Fidelity that was not

`4752f83`. Neither QEMU launcher passed `-nodefaults`, and `Profile::Metal` is
the only profile with no `-netdev` — exactly the condition that re-enables QEMU's
default-device pass. The machine whose entire purpose is having no network had
one. Measured with `query-pci` on QEMU 11.0.2: without it, `00:02.0 8086:10d3`
(e1000e) plus a slirp backend, an `ide-cd` on the ich9-ahci and an
`isa-parallel`; with it, gone, and the guest's own PCI enumeration shows `00:02.0
1033:0194`, the xHCI, in the freed slot.

None of it was visible to the argv assertion, **because these devices never
appear in argv**. The failure it was heading for is concrete: write the e1000e
driver the T14 needs and metal-sim's netd claims QEMU's phantom NIC, at which
point the "daemons exit on a machine with no device" half of what the config
certifies silently stops being exercised. Two more from the same commit: the
USB-HID tooth was a name enumeration (`contains("usb-kbd") ||
contains("usb-tablet")` while `scan_ports` binds any boot-protocol HID), and
`fn virtio(self) -> bool { self != Profile::Metal }` was fail-open for every
future variant.

## What QEMU structurally could not find

All live on the T14; none reachable from the dev host. None was found by a test —
every one came from reading the code against the hardware spec.

| Defect | Why QEMU cannot see it | Commit |
|---|---|---|
| The xHCI scratchpad pointer array pointed at a single buffer at `OFF_SCRATCH+2048` — not page aligned as §4.20 requires, and overlapping slot 2's output context at `0xA000`; and one buffer was written however many the controller asked for | QEMU reports zero scratchpad demand; the T14's Intel controller reports nonzero | `5bb673c` |
| `OP_PAGESIZE` had exactly one read in the kernel and it was inside a `log!`, while `PAGE` was a hardcoded 4 KiB and the scratchpad array written one `PAGE` apart. At bit 1 with `max_scratchpad = 8`, entry 7 lands at `0xF000` and the controller writes into block 0's interrupt ring | QEMU answers bit 0 | `71940c1` |
| `serial::init`'s loopback `assert!` killed the kernel ~20 instructions into `kernel_main`. Absent hardware reads `0xFF`, which passes the THR-empty test *and* reports "receiver ready" — wrong, not merely useless | Every dev config has a 16550, until `--metal-sim` existed | `2e52e8e` |
| The framebuffer memory-type gate classified only the entry holding the scanout's **first byte**, so a map whose tail falls in `BootServicesData` passes and the panic-path write lands in heap the PMM later hands out | QEMU's memory map is well-formed (filed, not fixed) | `938c7ac` |
| The I/O APIC reported success on a machine with nothing wired up, three ways: `> 0xFF` let 0xFF through, which is the broadcast encoding; `entries` from an unchecked `REG_VER`, and an undecoded MMIO window reads `0xFFFFFFFF` (256 entries, all-ones passes a mask read-back, `route` writes into the void behind a green log); and `route` never read its entry back | One textbook unit at `0xFEC00000` with identity GSIs | `09f40be` |
| "Every i8042 write is read back" was false for the one write that arms the pin. A controller that drops `CFG_PORT1_IRQ` still fills the output buffer and never asserts — invisible from every angle. `aux_reenable` was wrong three further metal-only ways | QEMU's controller never drops a config write and never resets itself | `442f3e8` `2716874` |
| The mouse framer's "resync within ≤2 packets from any offset" was false: bit 3 is set in every legal head byte *and* in `dx = 10`, so a one-byte misframe self-sustains. A TrackPoint pushed right reports upward motion with the right button held, indefinitely | The host test certifying it used `packet(0, 5, 7)` — the unique delta pair whose body bytes cannot masquerade as heads | `01f46c5` |

## A safety fix that shipped a new bug

`995f3cb` landed the §6.4 tripwire — an always-on assert catching a lock held
across a context switch, teeth proved both directions, gate A green. Its own test
introduced a **userland-reachable machine stall**.

`SYS_DEBUG` action 2 takes a private spinlock and calls `yield_now`, panicking
with the `LockGuard` alive — which is the point. But the kernel does not unwind,
so `LockGuard::drop` never runs and the lock stays held for the rest of the boot.
A second call spun `Lock::lock`'s full 500 M iterations with `IF=0` (MSR_FMASK
masks it on syscall entry, nothing on that path re-enables) and preemption
disabled: at `-smp 1` the timer drain, the idle-loop drain and the harness
heartbeat are all frozen for that window. `SYS_DEBUG` is ungated, so any process
could do it repeatedly. `4d86e32` arms the action once per boot and adds the case
where the second child must survive.

The same commit's attribution assertion was the vacuous needle above. One
hardening commit shipped both a new denial of service and a certification that
could not fail, with its suite green and its evidence honest.

## Reviews that were wrong

Adversarial passes over-fire, and a record listing only hits is a monument rather
than a record. The verifier step is what makes the method usable.

**The timer-vs-i8042 deadlock — filed twice, wrong both times.** CLAUDE.md
carried it as live at `0d2a324` ("`timer_handler` → `xhci::poll_if_pending` →
`XHCI.lock()` can deadlock against the same CPU's `fd.rs` keyboard/mouse poll. A
`try_lock` in the timer path removes it"), and the M2 review re-filed it against
the new i8042 drain, which holds exactly those locks for longer. The implementer
read the assembly instead: `timer_entry` tests CS in the interrupt frame and
branches to the Ring 0 stub *before* `call {handler}`, so the Rust handler only
ever runs on a tick that interrupted user code, where the CPU holds no `Lock` at
all. The recorded `fd.rs` instance could not fire for the same reason. The
proposed `try_lock` on `XHCI` would not have fixed the filed defect anyway — the
spin is on `HELD`, which `XHCI.try_lock()` reaches past. `86c372c` deleted the
redundant edge and turned the asm-branch argument into a checked
`assert_eq!(preempt::count(), 0)` at `kernel/src/arch/idt/timer.rs:113`, with
teeth: `cmp cs,0` in place of `test cs,3` trips it inside the drain window.

Also refuted in these waves, recorded so they are not re-derived: the
`enter_idle_loop` bypass in `schedule_no_return`; that the "live interrupter"
comment at `kernel/src/drivers/xhci/mod.rs:632` was wrong; a pre-`lidt` IF window
against `drivers/ioapic.rs`'s placement argument; that the tripwire's assert
directions were untested; and two findings about analyzer behaviour. These left
no commit, by construction — a refuted finding produces no diff. The timer
deadlock is the exception only because it had previously been *filed*, so its
retraction is in the tree.

One claim was half-wrong in a way worth keeping: the xHCI wave's reviewer held
that a keyboard survives the one-block pool. It does not — the survivor is the
boot stick, because QEMU puts a SuperSpeed `usb-storage` on the first SuperSpeed
port register ahead of every USB2 one, so no HID gets the block and "and
delivers" stays untested. `4b3cbd9` corrected `specs/metal-boot-plan.md`, which
had claimed the test proved the overflow "costs the extra devices and nothing
else", to say what is proved and what is not.

## A race that could not be staged, reported as such

`40aff72` fixed two xHCI event-demux defects — `wait_command` dropped any
non-Command-Completion TRB, permanently dearming a bound device's interrupt
endpoint; `wait_transfer` returned the *first* Transfer Event's code, so a
control transfer could complete on someone else's event — then tried to
demonstrate the race and could not. The window is one guest millisecond (the
six-device enumeration on `Profile::MetalUsb` runs `0.098` → `0.099`). Six boots
with the discard paths instrumented, 10,928 injected key-pair events per boot
with the i8042 removed so keys can only land on a USB keyboard, and separately
2,818 absolute-motion events per boot against a `usb-tablet` whose 1 ms endpoint
interval is the shortest on the bus: **zero** foreign `EVENT_TRANSFER` TRBs
reached either path. QEMU's main loop is what delivers injected input and runs
the retry timer, and it does not get in while the vCPU holds the BQL across
back-to-back xHCI MMIO for that millisecond.

The commit says the routing is argued by construction and not demonstrated by a
hit. That is the correct report. It also names what the stimulus *did* find,
unrelated and filed separately (`71dfe57`): a keyboard flood into a thread
blocked in `sys_read` panics the kernel at `toyos-sched/src/waitq.rs:124`.

## The rules

Each is paid for by evidence above. They are the payload of this file.

1. **A green suite is evidence the tests pass, not that the change is right.**
   Every wave in the tally was green when its review started.
2. **Prove teeth in both directions: break what a test guards, watch it go red.**
   Applied retroactively it found five vacuous certifications in M0 alone. A test
   earns its runtime by asserting something that can be false; if you cannot
   state what a run would have to observe to go red, it asserts nothing.
3. **The instrument is wrong before the subject is.** Gate A's four instrument
   defects, and here `metal_sim_compositor`, `audio_idle_suspend`, both xHCI
   tests, the 10 M-byte fuzz.
4. **Read the code, not the bug database.** The §6.4 asserts that never landed;
   the PT_TLS overflow reported as live; the panic-output issue fixed two days
   before it was retired; the timer deadlock carried as a known issue while the
   assembly said otherwise; `boot-image-split.md` describing deleted symbols.
5. **Verify against the artifact, not a proxy.** `query-pci`, not argv. The
   assembly, not the reviewer's summary. The diff, not the commit message. The
   `-device` values, not a name enumeration.
6. **An honest "I could not do this" beats a clean report.** `40aff72` reporting
   zero hits after six boots and ~13.7 k injected events each; `938c7ac` filing
   two confirmed findings outside the fix agent's task list rather than letting
   them vanish; `72c19bc` landing only the subset in files no in-flight agent
   held and naming the rest; `01c02e0` deleting an assertion whose only reachable
   outcome was a false red. The expected standard, not an exception.
7. **Adversarial passes over-fire; run a verifier and record the refutations.**
   Seven refuted findings above. A review process with no false positives is not
   adversarial enough; one with no verifier costs more than it saves.
