# CPU Attribution — Investigation

> 2026-07-29. Four investigations, three attribution models, one synthesis.
> **The premise was wrong.** The headline finding is that the recorded
> "~half the CPU is unattributed kernel time" is not a measurement of
> unattributed kernel time, and its sign is backwards.

## 1. What the gap actually is

HONEST HEADLINE: the "~50% unattributed kernel time" is not a measurement of unattributed kernel time, and after inspection we still do not know what produced the two numbers. What we DO know is enough to say the framing is wrong and to say what is genuinely unowned.

FIRST, A FINDING THAT RESHAPES EVERYTHING BELOW. The dossier and all three designers analysed a tree that no longer exists. Stage 7 — the cutover — is UNCOMMITTED IN THE WORKING TREE RIGHT NOW. Verified: `git status` shows `?? kernel/src/sched/` (untracked), `D kernel/src/waitq.rs` (staged delete), and `kernel/src/scheduler.rs` cut from 2075 lines at HEAD to 545 in the worktree. `kernel/src/sched/driver.rs` (559 lines) is the new pass. Every instrumentation site cited by every designer (scheduler.rs:1622, :1665, :1657, :1822-1959) is in code being deleted. Working-tree line numbers below are unstable — another agent is editing this tree.

WHAT THE 45-vs-97 GAP CANNOT BE (determined by inspection, high confidence). At HEAD, `ps` and the compositor taskbar are fed the SAME accumulator. `stop_cpu_timer` adds one delta to both the per-thread `cpu_ns` and the per-CPU `CPU_TIME_NS`; `total_cpu_ns()` (kernel/src/sched/driver.rs:102-105) sums the latter into sysinfo bytes 32..40 (kernel/src/arch/syscall.rs:1127,1141). Unattributed kernel time is absent from BOTH numerators, so it cannot open a gap between them — it pushes the 97% DOWN. True busy therefore EXCEEDS 97%, and the CLAUDE.md entry has the sign backwards.

WHAT IT IS MADE OF — reader-side, all verified by inspection, all at smp=1:
(1) Window mismatch, almost certainly dominant. `userland/toybox/src/ps.rs:54-56` computes `cpu_ns / uptime_ns * 100` where `uptime_ns` comes from sysinfo bytes 24..32 (`nanos_since_boot`, syscall.rs:1126) but `cpu_ns` accrues only from thread creation. `userland/compositor/src/main.rs:1512-1518` computes a correct 1-second delta ÷ 1-second delta. A lifetime average against an instantaneous sample. For a Doom launched mid-session this alone scales ps's number by (uptime − spawn_time)/uptime.
(2) Per-row flooring. `ps.rs:55` casts `f64 → u32`, a floor, once per printed THREAD row. system.toml boots compositor+soundd+netd+sshd before any shell; 12-20 rows costs 6-20 points on its own. Under-appreciated by two of the three designers.
(3) Reaped and zombie threads. `CPU_TIME_NS` is `fetch_add`-only with no decrement; `ps` enumerates only live `PROCESS_TABLE` entries (syscall.rs:1130-1134). Their time stays in the system total forever and vanishes from every row — an unbounded, monotonically growing divergence.
(4) NOT a term at smp=1: ps ignoring `cpu_count`. At smp=1, `total_available_ns = uptime × 1 = uptime`, which is exactly ps's denominator. The SINKS designer made this load-bearing; it is zero for the measurement actually reported.

WHAT IS GENUINELY UNOWNED (verified, and it is a different, smaller quantity). At HEAD: the pick-and-arm window (deliberate and documented at HEAD scheduler.rs:1613-1621), the idle-loop body, and `hlt` itself. There is still NO idle counter anywhere — grep for IDLE_NS/idle_ns across kernel/, toyos-abi/, userland/ returns nothing. So the guest cannot produce a "busy %" at all except as `total_cpu_ns`, which is the attributed sum. That strongly suggests the 97% came from HOST `top` on the QEMU process — a cross-axis comparison that also sweeps in QEMU's own emulation cost and the guest's ownerless idle-loop spinning. Provenance is unrecoverable: `git log -S "unattributed kernel time" -- CLAUDE.md` returns only the squash commit.

WHAT STAGE 7 CHANGES, AND THIS IS NEW. The cutover reshapes the answer. `pass()` (kernel/src/sched/driver.rs:302-305) calls `drain_irqs()` and THEN samples `now` once; `toyos-sched/src/cpu.rs:630-634` states the design intent ("the old scheduler read the clock about fifteen times mid-flight"). `dispatch(...self.now)` (cpu.rs:832) sets the task's `since` to the PASS-ENTRY timestamp, and `RunningTask::charge` (toyos-sched/src/task.rs:849-853) charges `now.since(since)` at the next pass entry. So a task's charge now spans [pass N entry, pass N+1 entry] and INCLUDES pass N's pick/arm/switch and pass N+1's `drain_irqs`. Consequently, post-cutover:
  - Dispatch and IRQ-drain overhead is MISattributed to the incoming task, not unattributed.
  - The only remaining truly unowned time is the interval where no task is current: `charge_cpu_time` (driver.rs:339-348) gates on `percpu::current_tid().is_some()` and simply DISCARDS the delta otherwise. That is the idle-loop body plus `hlt`, indistinguishable from each other.

STILL NEEDS MEASUREMENT (and needs no kernel change): the magnitude of terms (1)-(3). Two `ps` snapshots Δt apart, plus reading sysinfo bytes 32..48 that `ps` already fetches and ignores, settles it. `header total_cpu_ns − Σ(printed cpu_ns)` is exactly the reaped+zombie+transit loss.

## 2. How much is a TCG artefact

SHORT ANSWER: essentially none of the reported ~50% is a TCG artefact — because essentially none of it is what CLAUDE.md says it is. The TCG question only becomes live once you are measuring the real (and much smaller) scheduler overhead. Separately, there is a LARGER and more certain non-TCG distortion that all three designers under-weighted.

TERM BY TERM.

0% TCG, holds identically on hardware:
- Every reader-side term (window mismatch, per-row flooring, reaped threads). Pure arithmetic in `userland/toybox/src/ps.rs:54-56` versus `userland/compositor/src/main.rs:1512-1518`.
- The EXISTENCE of every unattributed window. Pure control-flow facts.
- The build-configuration asymmetry (below).

THE DISTORTION NOBODY WEIGHTED HIGHLY ENOUGH, and it is not TCG. The kernel is built unoptimized with overflow checks while userland is optimized. `kernel/Cargo.toml` has NO `[profile]` section and `kernel/` is its own workspace, so the dev default applies: opt-level 0, debug-assertions, overflow-checks. `userland/Cargo.toml` sets `[profile.dev.package."*"] opt-level = 2` — EVERY dependency — plus doom and compositor explicitly at 2. And the empirical proof, which no designer produced: `kernel/target/x86_64-unknown-none/` contains only `debug/`. **A release kernel has never been built in this tree.** `src/build.rs:317-318` makes `--release` opt-in and `cargo test` never passes it.
The evidence is right there in the disassembly: `clock::nanos_since_boot` (verified at 0xe86b0 in the shipped binary, NOT 0xede00 as all three designers claimed) does `call rdtsc` OUT OF LINE despite `#[inline]`, plus two out-of-line `call AtomicU64::load`, plus two overflow-check branches, plus `call __udivti3.__stub` (0x248078). At opt-level 2 most of that collapses.
For any absolute kernel-versus-userland split, this is a larger and far more certain inflation of the kernel's share than TCG is, and it is one flag to fix.

TCG-DISTORTED, MAGNITUDE ONLY, AND NON-UNIFORMLY — this is the part that matters once real scheduler overhead is measured. "TCG inflates kernel work" is false as a blanket claim; it runs in both directions:
- Heavily INFLATED: the three x2APIC `wrmsr` per dispatch in `apic::arm_one_shot`. HEAD scheduler.rs:1615-1618 says so outright ("each an exit to the device model under TCG").
- Heavily INFLATED, and on a path hardware never takes: the CR3 reload. CLAUDE.md records that TCG supports neither PCID nor INVPCID, both CPUID-gated, so TCG takes the full-flush fallback a 2020+ machine would not.
- UNDER-stated by TCG, i.e. WORSE on hardware: device MMIO. `specs/plans/arm64-research-2026-07-28.md:46` measures 168 ns in TCG's in-process device model versus 876 ns under HVF — a 5.2x regression off TCG. Anything MMIO-bearing (the timer handler's virtio-console log drain, xHCI polls) grows on real hardware.
- Roughly NEUTRAL, share survives: plain compute — `drain_irqs`, the pick, BTreeMap work, the `__udivti3` divide.

THE PRIME SUSPECT IS DEAD TWICE OVER. CLAUDE.md names "context-switch volume with address-space switches, expensive under TCG". Using the project's own measurement — 1619 ns per address-space switch + TLB flush under x86-64 TCG (`specs/plans/arm64-research-2026-07-28.md:41`) at ~1500 switches/s — that is 0.24% of one core. Reaching 50% would need ~333 µs per switch, roughly 200x the measured cost. And the same doc gives 251 ns native, so on hardware it is ~0.04%. Second refutation, structural: at HEAD the `mov cr3` runs AFTER `start_cpu_timer` (scheduler.rs:1622 then :1634), so its cost was already charged to the incoming task, never lost.

CONSEQUENCE FOR "IS OPTIMISING THIS WORTH ANYTHING". No. There is no 50% scheduler cost to optimise. The value of this work is attribution, not speed — and the write-up must say that plainly, in the memory-ownership spec's discipline. The one thing worth doing for speed is orthogonal and cheap: build the kernel release at least once and re-measure, because that is a several-fold effect on the kernel's apparent share and costs one flag.

REPORTING RULE. Until a second platform exists, any bucket whose body contains an MSR write, a CR3 write, or MMIO is reported FLAGGED, and the split is published as a ranking plus a ratio, never as a hardware percentage, and never as the Stage 7 baseline. The counters make the TCG-versus-hardware ratio itself a falsifiable prediction checkable on the first KVM boot — which is more than the current numbers offer. Note this host cannot check it: `src/qemu.rs:7-13` selects `-accel kvm` only when `/dev/kvm` exists and the arch is x86_64, and this is darwin/arm64.

## 3. Recommended attribution model

TAKE THE MODEL FROM "SUBSTRATE"/"PRECISE" (exact, gapless, per-CPU, raw-tick-friendly, reusing timestamps that already exist), THE VALIDATION FROM "PRECISE" AND "OWNERSHIP" (known-answer injection plus omission control), AND REJECT SAMPLING AS THE PRIMARY INSTRUMENT. Then shrink all three drastically, because Stage 7 has already built most of the substrate they proposed.

WHY SAMPLING LOSES, ON ITS OWN EVIDENCE. The sampled designer's strongest finding refutes the sampled design: syscalls run IF=0 end to end (`MSR_FMASK = 0x40200`, kernel/src/arch/syscall.rs:31) and device ISRs never `sti` (kernel/src/arch/idt/msix.rs:17-19). A LAPIC-timer sampler is therefore blind to most of the ToyOS kernel, and the LAPIC LVT timer cannot be NMI-delivered. That finding should be recorded in CLAUDE.md — Layer 3 as the roadmap specifies it is NOT PORTABLE to this kernel — but the correct conclusion is not to build it as the primary instrument. It also cannot supply an exact identity, and this project's fail-fast principle wants an invariant, not a statistic with a confidence band.

WHY THE FULL "OWNERSHIP" LIFT LOSES, ON ITS OWN EVIDENCE. It concedes that enforcement degrades from compile error to runtime budget assert, and that `Sched` becomes a junk drawer by accretion with only social controls against it. It also depends on a `Subsystem` enum that does not exist — verified, no `enum Subsystem` anywhere in kernel/, toyos-abi/, toyos/. Right idea, wrong time: it is a speculative dependency on an unbuilt spec, layered on top of an uncommitted cutover.

THE RECOMMENDED MODEL, ADAPTED TO THE POST-CUTOVER TREE.

The right home is `Machine` in kernel/src/hw.rs. It already owns `now()`, `halt()` (hw.rs:101-103), `set_timer()` and `trace()`, and its own doc says "the simulator replaces this file and nothing else" — so accounting placed there is consistent between kernel and simulator by construction. This is the boundary Stage 6 built and nobody proposed using.

TIER 1 — close the identity. Three per-CPU counters that TILE the timeline, reusing the existing cache-line-padded `CpuTime` type (kernel/src/sched/driver.rs:80-82):
  TASK_NS      — pass-to-pass intervals where a task was current (this is today's CPU_TIME_NS)
  IDLE_EXEC_NS — pass-to-pass intervals where no task was current, minus halt
  HALT_NS      — measured inside `Machine::halt`
Identity, per CPU, exact integer equality: `TASK_NS + IDLE_EXEC_NS + HALT_NS == now − online_since`.

The reason this is ~10 lines and not the 75-90 the designers budgeted: `charge_cpu_time` (kernel/src/sched/driver.rs:339-348) ALREADY computes the exact delta and already DISCARDS it when `percpu::current_tid()` is None. The idle bucket is a two-line `else` branch on a value already in a register. Zero new clock reads on the pass path. HALT_NS needs two clock reads around `sti; hlt` — on the halt path, where cost is definitionally irrelevant. No designer proposed this; it is the cheapest correct thing available and it exists only because of the cutover.

TIER 2 — name the pass. Tier 1 leaves dispatch overhead MISattributed (folded into the running task's charge). To separate it you need a second timestamp per pass: one at entry (`now`, already sampled at driver.rs:305) and one after the pass body, before `execute(action)`. That yields PASS_NS and reduces the task's charge to task-run-only. Cost: ONE new clock read per pass. Perspective: HEAD's scheduler read the clock ~12-15 times per pass (hw.rs:31-35 calls that count "the honest size of that debt"); Stage 7 reads it once. Two reads is still a 6x reduction versus HEAD.

EXPLICITLY DO NOT BUILD NOW: the 8-bucket microstate machine with naked-asm gs-offset hooks in the syscall and IRQ entry stubs; the `Subsystem` lift; the sampler; per-syscall or per-fault duration timing. Reasons: (a) CLAUDE.md's ">2x or prefer the simpler solution" rule — Tier 1+2 answers the question asked; (b) the syscall/IRQ stubs are the highest-risk edit surface in the kernel and their payoff is the user/kernel split, which is a DIFFERENT question; (c) Stage 7 is uncommitted and churning adjacent code.

ONE THING WORTH RECORDING BUT NOT COUPLING. `nanos_since_boot` should store raw TSC and scale at read — illumos's `gethrtime_unscaled`/`scalehrtime` split. Verified necessary in principle (three out-of-line calls plus `__udivti3`), but it has ~338 call sites and its own blast radius. It is a separate, independently justified change. Tier 1 needs zero new clock reads, so it does not depend on it — which is precisely why Tier 1 should go first.

## 4. Making the residual visible

THE EMPIRICAL CASE AGAINST A DERIVABLE RESIDUAL, WHICH I VERIFIED AND WHICH SETTLES THE DESIGN QUESTION. Both `total_cpu_ns` (sysinfo bytes 32..40) and `total_available_ns` (40..48) have been written by `sys_sysinfo` (kernel/src/arch/syscall.rs:1141-1142) for the entire life of this defect. Their ratio IS the correctly-denominated attributed fraction. `userland/toybox/src/ps.rs` reads only bytes 24..32 and never touches 32..48; `free.rs` reads only 0..16. The one subtraction that would have exposed this was free, one line, and sitting in a buffer `ps` already fetches — and it went unperformed for months. That is the proof that a residual someone must derive is a residual nobody derives. The owner's complaint is exactly this failure mode.

FOUR MECHANISMS, IN INCREASING LOUDNESS.

(1) THE STATES ARE TOTAL, SO THERE IS NOTHING TO SUBTRACT. Today unattributed time is not under-counted, it is UNREPRESENTABLE: `charge_cpu_time` (kernel/src/sched/driver.rs:339-348) computes the delta and throws it away when no task is current. Making the three buckets tile the timeline means there is nowhere to put an unowned nanosecond. Note the direction this corrects, and it must be stated in the write-up because it contradicts the CLAUDE.md entry: true busy is `elapsed − HALT_NS`, and it will read HIGHER than 97%, not lower.

(2) THE ASSERT, EXACT AND FAIL-FAST — the direct analogue of specs/plans/memory-ownership-spec.md §3.3's reap-time `assert_eq!(account.charged(), 0, "pid {} exited owing {} bytes to {:?}")`. Per CPU: `assert_eq!(TASK + IDLE_EXEC + HALT, now − online_since, "cpu {cpu} ran {n} ns with no owner")`. Exact equality, no tolerance: every bucket is fed by differences of the same boundary stamps, so interval N's end is interval N+1's start and the sum telescopes identically. A tolerance is a place a bug hides.
PLACEMENT, AND THE OBVIOUS SPOT IS WRONG — SUBSTRATE caught this and I confirmed it survives the cutover. NOT `log_health`: its only caller is `idle_loop` (kernel/src/sched/driver.rs:427-431), gated on `IDLE_HEALTH_COUNTER % 1000 == 999`, so it is MUTE UNDER EXACTLY THE SATURATING LOAD BEING INVESTIGATED. Put it at the `pass()` tail under `feature = "check"`, which runs 1000+/s under load and rarely at idle — the inverse duty cycle of the bug. This matches specs/scheduler-core-spec.md:776's "cheap subset at kernel pass ends".

(3) IT IS ON SCREEN, ONCE PER SECOND, WITH A NAME. The compositor taskbar is the only in-guest busy meter and it is the sole consumer of sysinfo bytes 32..48 (userland/compositor/src/main.rs:1509-1518, rendered at :568 as "CPU {}%"). It already calls the field `busy`. Writing `available − halt` there instead of `total_cpu_ns` makes the meter honest with no ABI change and no other consumer edit. Render `CPU 97% (38 sched)` — the gap is not something a human computes, it is a labelled number.

(4) IT IS A ROW IN `ps`, IN THE SAME PERCENT COLUMN. Synthetic rows `[sched]`, `[idle]`, `[halt]`, `[reaped]` printed below the process list, participating in the same column, which then sums to 100 by construction — or the assert already fired. `[reaped]` is computable TODAY with zero kernel work as `header total_cpu_ns − Σ(printed cpu_ns)`.

AND FIX THE READER, BECAUSE THAT IS WHERE THE REPORTED GAP ACTUALLY LIVES. None of the above touches ps's lifetime-average-over-since-boot-uptime denominator or its per-row `as u32` floor. Those are userland bugs and they are, on the evidence, most of the 52 points. They must ship alongside, or the new instrument will be blamed for disagreeing with a number that was always wrong.

THE HONESTY LIMIT, STATED UP FRONT. An exact conservation identity CANNOT detect MISattribution. It closes just as perfectly if pass time is charged to the wrong task — and under Stage 7 that is the live defect, not a hypothetical. Totality is necessary and not sufficient; the sufficient part is validation, below. The model's honest claim is: it makes unowned time impossible and misowned time reviewable.

## 5. Cost

LINES.
Tier 1 (close the identity): ~10-15 lines of kernel Rust — a two-line `else` branch in `charge_cpu_time` (kernel/src/sched/driver.rs:339-348), two clock reads and a `fetch_add` inside `Machine::halt` (kernel/src/hw.rs:101-103), two new `CpuTime` statics reusing the existing padded type (driver.rs:80-82). Plus ~10 lines of `sys_sysinfo` change and ~25 lines of userland reader (ps rows + the ps denominator fix + the taskbar label).
Tier 1 assert: ~8 lines under `feature = "check"`.
Tier 2 (name the pass): ~20 lines — one extra clock read per pass and one more counter.
Tests (see validation): ~150 lines in tests/toyos-rust-tests.
Total to a complete, read, asserted instrument: roughly 60 lines of kernel plus 40 of userland plus 150 of tests. Against 250-300 kernel lines plus asm for the sampled model and ~90 lines plus eleven naked-asm hooks for the full precise model.

INSTRUCTIONS ON FAST PATHS.
Syscall path: ZERO. Nothing is added. This is the single biggest divergence from all three designs, two of which put instructions in `syscall_entry`'s naked stub.
Interrupt path: ZERO. Nothing is added to `msix_entry!` or `timer_entry`.
Scheduler pass: ZERO for Tier 1 (the delta is already computed and discarded). ONE additional `nanos_since_boot` per pass for Tier 2. For scale: HEAD's scheduler read the clock ~12-15 times per pass — kernel/src/hw.rs:31-35 names that count "the honest size of that debt" — and Stage 7 reads it once. Tier 2 leaves the pass at 2 reads, still ~6x cheaper than HEAD.
Halt path: two clock reads per halt. Cost irrelevant by definition; the CPU is about to stop.
Cost of one clock read, verified from the shipped binary rather than assumed: `clock::nanos_since_boot` at 0xe86b0 is a 0x40-byte frame + out-of-line `call rdtsc` + `call AtomicU64::load` (TSC_BOOT) + a sub-overflow branch + `call AtomicU64::load` (TSC_PERIOD_FS) + `mul` + a mul-overflow branch + `call __udivti3.__stub` (compiler_builtins software 128-bit division, 0x248078). Three out-of-line calls and a libcall divide. Note this is an opt-level-0 measurement (see below), so it is an upper bound.

MEMORY. Two additional `[CpuTime; MAX_CPUS]` arrays, 64-byte aligned = 128 bytes per CPU. Against the existing per-CPU trace ring at 4096 × 24 = 96 KiB (kernel/src/trace.rs). Negligible.

DOES THIS GROW THE KERNEL? YES, and I will not soften that. It adds ~60 kernel lines, two statics, and one clock read per pass at Tier 2, and it deletes nothing. By CLAUDE.md's zero-technical-debt rule that is only justified if the counters are actually READ — which is why the reader (ps rows plus the taskbar relabel) is part of Tier 1 and not deferred. A counter nobody reads is exactly the defect this project forbids, and the tree already has one: `TRACE_RINGS` (kernel/src/trace.rs:134) has NO reader anywhere — verified, grep across kernel/, tests/, .claude/, src/, toyos-sched/ finds only the definition and the writer. Do not add a second write-only instrument.

WHAT IT DOES NOT COST: no new hardware timer, no PMU dependency, no lock, no allocation on any instrumented path, no ABI break (sysinfo bytes 32..40 are redefined, and the compositor is their only consumer and already calls the field `busy`), no naked-asm edits, no new per-CPU struct fields, no new hardcoded gs offsets.

IT MAKES NOTHING FASTER. Same disclaimer as specs/plans/memory-ownership-spec.md §6, stated up front rather than discovered later. It closes attribution. The arithmetic above says it will most likely show that the suspected cost is not the cost.

ONE CHEAP THING THAT DOES MAKE SOMETHING FASTER, AND IS NOT PART OF THIS: build the kernel with `--release` at least once. `kernel/target/x86_64-unknown-none/` contains only `debug/` — it has never been done. That is one flag and a several-fold effect on the kernel's apparent share.

## 6. Validation

THE GOVERNING DISTINCTION, AND IT MUST BE STATED IN THE COMMIT MESSAGE: proving the accounting is TOTAL is easy and nearly worthless; proving it is CORRECT requires an answer derived from outside the instrument. A bug that charges user time to the `sched` bucket passes conservation perfectly.

V0 — DO NOT PORT I7. This is a new finding and it is sharp. `toyos-sched/sim/src/invariants.rs:305-307` asserts `Σ finalized.cpu_ns == Σ vm.busy_ns`, scoped to "sim" at specs/scheduler-core-spec.md:786, and it is a tautology there because the simulated pass consumes zero virtual time. All three designers warned about that. NONE noticed that the cutover PROPAGATES the tautology into the kernel: `toyos-sched/src/cpu.rs:630-634` samples `now` ONCE per pass and threads it as a value, so every residency transition in a kernel pass happens at the same instant and the pass costs zero ACCOUNTED time by construction. Ported unchanged, I7 will be green in the kernel and will prove nothing. The kernel identity must be against the WALL CLOCK (`now − online_since`), never against the sum of the pass's own charges.

V1 — KNOWN-ANSWER INJECTION, WITH A SLOPE RATHER THAN A POINT. Ground truth set by the harness, not read from the instrument.
  - A pure userspace spin of N ms with no syscalls, on a quiet smp=1 guest → TASK_NS gains N ms ±1%, HALT_NS ~0.
  - `nanosleep(N ms)` on an otherwise idle machine → HALT_NS gains ~N ms, the task's cpu_ns gains ~0. Catches a sign error in the idle path.
  - THE STRONG ONE: a `sys_yield()` loop at rate R, swept R, 2R, 4R → PASS_NS must grow LINEARLY and the SLOPE is the per-pass dispatch cost, a number the instrument was never told. A constant offset is per-transition overhead (expected, and this measures it); a slope error is a real attribution bug. An instrument that recovers a slope it was not given is measuring rather than asserting.

V2 — OMISSION CONTROL. The test I would refuse to ship without, taken from the ownership design, which is the only one that proposed it. Burn a known 5 ms with the charge site DELETED, and assert the time lands in the residual/idle bucket rather than being silently absorbed by a lexical neighbour. Without this, the residual bucket is entirely untested and the whole visibility claim in section 4 is unvalidated.

V3 — ADVERSARIAL, AND IT FAILS ON THE CURRENT TREE. `drain_irqs()` runs at kernel/src/sched/driver.rs:304, BEFORE `now` is sampled at :305 — so waking other processes' threads and posting their io_uring completions falls inside whoever was current. Test: process A idles in a loop; process B floods the network completion path; assert A's charge does not grow. THIS FAILS ON THE WORKING TREE TODAY. Its passing is the proof the fix landed. One test that only passes after the change is worth more than ten that pass on both sides.

V4 — NEGATIVE CONTROL, THE GATE MUST GO RED. This project already demands this discipline and CLAUDE.md records it as "two proofs that the harnesses have teeth, both required": `TOYOS_LOOM_RAW=1 ... --features no-preempt-guard` must FAIL, and the sim's `old_steal_port` scenario must fail. Here: a build with one charge site removed must make the conservation assert fire. If removing an owner does not turn the gate red, the gate is decoration.

V5 — DIFFERENTIAL CROSS-CHECK, WHICH GOT CHEAPER AND NOBODY NOTICED. The per-CPU buckets and the per-task `TaskAccounting` are produced by different mechanisms. Assert `Σ_task cpu_ns ≈ TASK_NS` within one transition per pass. Stage 7 already made the read side safe: `task_cpu_ns` is now a published atomic (kernel/src/sched/payload.rs:119-120, kernel/src/scheduler.rs:458-463), NOT the old O(threads × cpus) walk taking every CPU's blocking queue lock under the global PROCESS_TABLE. Two designers nominated that walk as the cause of CLAUDE.md's unreproduced "ps stalled >2s" and proposed working around it; the cutover already fixed it.

V6 — REGRESSION, WITH A HARD PREREQUISITE. Gate A's thorough tier (`cargo test --test toyos-build -- --audio-gate 30`) is the right gate, because Stage 6 proved this exact class of accounting change moves audio (21% of soundd's wakes). BUT GATE A IS CURRENTLY BROKEN AND MUST BE FIXED FIRST. Commit 9b1ba35 records the thorough tier PASSING while printing `wake_lat 153766519us` — 153 seconds of lateness in a 3-second test, an instrument fault that Mann-Whitney's rank-based comparison cannot see, and whose toml-ready output would set the next ceiling to 2× an impossible value and disable the check permanently. Commit 2c3753c records a second false-red and, more importantly, "`wakes` at 314 against a recorded 426-496 is the sharper anomaly here, already collected and ungated". Validating a CPU-attribution change against an instrument that ranks impossible values and would re-baseline itself broken is worse than not validating it.

WHAT NONE OF THIS PROVES. No test here proves the bucket assignment is what a reader would expect for paths V1 does not cover. Conservation is blind to it and known-answer injection only covers what someone thought to inject. The mitigation is that a wrong bucket is a READABLE wrong bucket — a reviewer can see at the charge site which bucket page-fault time goes to, which is not true of a sampled histogram.

## 7. Sequencing

THE PREMISE OF THE QUESTION HAS ALREADY EXPIRED. Stage 7 is not upcoming; it is UNCOMMITTED IN THE WORKING TREE. Verified: `?? kernel/src/sched/` untracked (driver.rs 559 lines, mod.rs, payload.rs, waitqs.rs), `D kernel/src/waitq.rs` staged for deletion, `kernel/src/scheduler.rs` cut from 2075 lines at HEAD to 545, and modifications across 14 more kernel files plus toyos-sched. `retire_task` in the worktree carries the comment "Stage 7a keeps this synchronous... Stage 7b makes it a bare message." Another agent is mid-cutover in this tree right now.

CONSEQUENCE: "instrument before the cutover" is no longer available, and attempting it would be actively harmful. Every instrumentation site in all three designs sits in code that is being deleted, and any kernel edit now collides with an in-flight rewrite of the same files.

RECOMMENDED ORDER.

STEP 1 — NOW, USERLAND ONLY, ZERO CONFLICT. Fix `ps`: divide by a two-snapshot delta instead of since-boot uptime, stop flooring each row to an integer, and print `[reaped]` as `header total_cpu_ns − Σ(printed cpu_ns)` from bytes 32..48 it already fetches (userland/toybox/src/ps.rs:36,49,54-56). This touches no file the cutover touches, needs no kernel change, and lands in the part of the system where the reported 45-vs-97 gap actually lives. It is also the test of the whole diagnosis: if the gap largely closes, the CLAUDE.md entry gets corrected rather than acted on.

STEP 2 — NOW, AND URGENTLY, IN PARALLEL. Fix gate A's own two defects (commits 9b1ba35 and 2c3753c): add an implausibility bound separate from the regression bound, and investigate the ungated `wakes 314 against a recorded 426-496`. The migration's validation instrument is currently producing impossible values and would re-baseline itself broken. Everything downstream — including judging Stage 7 — depends on this gate, and a broken gate is worse than no gate because it grants false confidence.

STEP 3 — AS THE LAST COMMIT OF THE CUTOVER, OR IMMEDIATELY AFTER IT LANDS. Tier 1, the three-counter identity plus the assert. This is the argument for AFTER rather than BEFORE, and it is not a concession: the cutover CREATES the substrate. Stage 7's "one timestamp per pass, threaded as a value" (toyos-sched/src/cpu.rs:630-634) is what makes the identity ~10 lines instead of the 75-90 all three designers budgeted against the old scheduler, and `charge_cpu_time` already computes and discards the exact delta the idle bucket needs. Doing this before the cutover means writing it twice and throwing the first one away.

STEP 4 — Tier 2 (name the pass), then the before/after comparison.

HOW TO SALVAGE A BEFORE/AFTER, WHICH IS THE REAL LOSS HERE. The clean measurement is already gone: the cutover is written and no attribution existed before it. The salvage is that HEAD is a commit. Land Tier 1 on a branch off HEAD as well as on the cutover, then A/B the two — same host, same session, per CLAUDE.md's warning that concurrent measurement in this tree is unreliable and that one should A/B against the same HEAD rather than compare to a number someone recorded earlier. That is the only way Stage 7's efficiency justification becomes checkable rather than asserted.

THE SPECIFIC THING TO CHECK FIRST IN THAT A/B, AND IT IS TIME-CRITICAL. Stage 7 appears to REINSTATE the misattribution Stage 6 deliberately removed. At HEAD, `start_cpu_timer` takes a deliberately fresh sample AFTER the pick and arm, and the comment at scheduler.rs:1613-1621 explains why: dating the charge earlier "bills that per *dispatch*, so it lands on whichever task is dispatched most often; measured, that is soundd, and it cost 21% of its wakes on `audio_tone_load` at smp=1". In the worktree, `dispatch(...self.now)` (toyos-sched/src/cpu.rs:832) sets the task's `since` to the PASS-ENTRY timestamp and `RunningTask::charge` (toyos-sched/src/task.rs:849-853) charges to the next pass entry — so each dispatch's arming cost is billed to the incoming task again, and the same `ns` is fed to the fair-share vruntime charge (cpu.rs:646-651). That is a regressive per-dispatch tax on frequently-woken tasks, which is soundd. Commit 2c3753c already records an unexplained `wakes 314 against a recorded 426-496`. That is the same signature. Whether or not the attribution work proceeds, this should be checked against gate A before the cutover is committed — it is a fairness regression, not merely an accounting one.

## 8. What the designers missed

THE ONE THAT MATTERS MOST: ALL THREE ANALYSED A TREE THAT NO LONGER EXISTS. None noticed `?? kernel/src/sched/` in the working tree, that `kernel/src/waitq.rs` is staged for deletion, or that scheduler.rs has gone from 2075 lines to 545. Every instrumentation site in every design targets deleted code. This alone invalidates all three implementation plans as written, and it was one `git status` away.

VERIFIED FACTUAL ERRORS.

1. The disassembly address is wrong in all three. Each independently reported "`nanos_since_boot` at 0xede00" as a verified disassembly. 0xede00 in the shipped binary is a hashbrown `RawTable::reserve`; the real symbol is at 0xe86b0. The SUBSTANCE is correct — three out-of-line calls plus `call __udivti3.__stub` at 0x248078 — but three designers presented a specific address from a stale build as first-hand evidence. Right conclusion, fabricated provenance, and the identical error in three places suggests it was copied rather than run.

2. PRECISE's stated most-likely-to-break-something risk is already mitigated. It claims adding per-CPU fields means new hardcoded gs offsets with "no compile error", and that "the codebase does not currently do [offset asserts] for ANY of its existing hardcoded offsets". False: kernel/src/arch/percpu.rs:175-189 has fourteen `const _: () = assert!(core::mem::offset_of!(PerCpu, ...) == N)` covering exactly the fields in question.

3. SINKS made "ps ignores cpu_count" load-bearing; it is zero for the measurement reported. At smp=1, `total_available_ns = uptime × 1 = uptime`, which is precisely ps's denominator. The ncpu term cannot contribute to a `--smp 1` observation.

WHAT NOBODY FOUND.

4. STAGE 7 RE-CHARGES DISPATCH OVERHEAD TO THE INCOMING TASK, and there is already an unexplained gate-A signature matching it. `dispatch(...self.now)` (toyos-sched/src/cpu.rs:832) dates the charge from pass entry, before the pick and arm; `RunningTask::charge` (toyos-sched/src/task.rs:849-853) closes at the next pass entry; the same `ns` feeds the fair-share charge (cpu.rs:646-651). This is exactly what HEAD scheduler.rs:1613-1621 says cost soundd 21% of its wakes, and commit 2c3753c records "`wakes` at 314 against a recorded 426-496... already collected and ungated". Connecting those is the most actionable finding in this review and no designer made it. It is a FAIRNESS regression, not just an accounting one.

5. STAGE 7 PROPAGATES I7's TAUTOLOGY INTO THE KERNEL. All three warned that I7 is vacuous in the simulator. None noticed that `now` is now sampled once per pass and threaded as a value (toyos-sched/src/cpu.rs:630-634), which reproduces the zero-cost-pass property in the real kernel. Ported unchanged, I7 will be green and prove nothing. This is precisely the failure they each warned about, arriving by a route none of them anticipated.

6. THE KERNEL HAS NEVER BEEN BUILT RELEASE. `kernel/target/x86_64-unknown-none/` contains only `debug/`. SINKS correctly inferred the profile from Cargo.toml but produced no empirical proof and then ranked the finding below the ps bug. For any ABSOLUTE kernel-versus-userland split — which is what "half the CPU is kernel time" is — an opt-level-0 kernel with overflow checks against an all-opt-level-2 userland (`[profile.dev.package."*"] opt-level = 2`, plus doom and compositor) is a larger and far more certain distortion than TCG, and costs one flag to remove.

7. THE CHEAPEST CORRECT FIX WENT UNPROPOSED. `charge_cpu_time` (kernel/src/sched/driver.rs:339-348) already computes the exact delta and discards it when no task is current. The idle bucket is a two-line `else` branch on a value already in a register. The designers budgeted 75-300 lines against the old scheduler; the cutover reduced it to about ten.

8. `Machine` IS THE OBVIOUS HOME AND NOBODY USED IT. kernel/src/hw.rs already owns `now()`, `halt()`, `set_timer()` and `trace()`, and its doc says the simulator replaces this file and nothing else — so accounting placed there is consistent between kernel and simulator by construction. Two designs proposed naked-asm hooks scattered across six files instead.

9. STAGE 7 ALREADY FIXED THE PERTURBATION DEFECT TWO DESIGNS PROPOSED WORKING AROUND. `task_cpu_ns` is now a published atomic read (kernel/src/sched/payload.rs:119-120), not an O(threads × cpus) walk taking every CPU's blocking queue lock under the global PROCESS_TABLE.

WHAT EACH GOT RIGHT AND SHOULD BE KEPT.
- SAMPLED's best work refutes its own model, and its author nearly said so: syscalls run IF=0 end to end (`MSR_FMASK = 0x40200`, kernel/src/arch/syscall.rs:31) and device ISRs never `sti` (kernel/src/arch/idt/msix.rs:17-19), so a LAPIC-timer sampler is blind to most of the ToyOS kernel and the LVT timer cannot be NMI-delivered. That belongs in CLAUDE.md as a correction: Layer 3 as the Diagnostics roadmap specifies it is NOT PORTABLE to this kernel. Also keep its insight that a deficit residual cannot be closed by relabelling.
- SUBSTRATE correctly caught that the assert must not go in `log_health` — its only caller is the idle loop (kernel/src/sched/driver.rs:427-431, gated `% 1000 == 999`), so it is mute under exactly the load being investigated. Confirmed to survive the cutover.
- OWNERSHIP contributed the two best items: the omission-control test (assert that removing a charge site puts the time in the residual, which is the only thing that validates the residual bucket at all), and the empirical argument that a derivable residual is an invisible one — sysinfo bytes 32..48 have been available for the entire life of this defect and no userland program reads them. Its own failure-mode section is also honest enough to defeat the rest of its proposal.
- ACCOUNTING and PRIOR-ART correctly identified the per-row `as u32` floor (userland/toybox/src/ps.rs:55) as a large term the others under-weighted.

FINALLY, ONE THING ALL FOUR ANALYSES AND CLAUDE.md SHARE: nobody could reproduce the 97%. There is still no idle counter anywhere in the tree, so the guest cannot produce a busy figure except as the attributed sum. The number was most likely host `top` on the QEMU process — a cross-axis comparison that also sweeps in QEMU's own emulation cost and the guest's ownerless idle-loop spinning. That should be written into CLAUDE.md as the correction, because an unrecoverable-provenance number driving a kernel redesign is the same defect class the task was commissioned to fix.
