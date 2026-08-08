# The user machine state

**The invariant, which nobody had written down:**

> A transition out of Ring 3 that can reach another task must save and restore
> **the whole** user machine state, and a task that has never been in Ring 3
> must start from a **declared** state.

This document is what that sentence means on x86-64 as this kernel configures
the machine, the two live defects that violate it, and the design that makes the
class unrepresentable rather than fixed twice.

It is not about "the FPU". The FPU is where the two instances happen to live.
The question is what a ring transition preserves, and the answer has to be
*everything*, stated once, in one place, with no way to say "some of it".

---

## 1. What the whole state is, on this machine

The state a Ring 3 thread can observe and a Ring 0 excursion can disturb:

| component | in the state? | why |
|---|---|---|
| general-purpose registers | yes | every entry already saves what it clobbers |
| `RIP`, `RSP`, `RFLAGS`, `CS`, `SS` | yes | the `iretq` frame or `sysretq`'s `rcx`/`r11` |
| `FS.base` (TLS) | yes | `hw.rs` swaps it with `rdfsbase`/`wrfsbase` on every context switch |
| XMM0–15, `MXCSR` | **partly** | three of five Ring 3-reachable entries save it — §3 |
| x87 registers, `FCW`, `FSW`, `FTW`, `FIP`, `FDP`, `FOP` | **no** | nothing anywhere — §2 |
| YMM/ZMM, opmask | not yet | `XCR0` is 1, so they do not exist — §5 |

`XCR0` is 1 because `CR4.OSXSAVE` is set nowhere: neither `kernel/src` nor
`bootloader/src` contains an `xgetbv` or an `xsetbv`, so the machine runs with
the reset value of `XCR0`, which permits x87 and SSE and nothing else. AVX
`#UD`s. Therefore **`FXSAVE64`/`FXRSTOR64` is a *complete* save of everything
this kernel permits to exist**, not a cheap approximation of one.

The kernel itself contributes nothing to that state: it is soft-float by
compiler guarantee. `rustc +toyos --print cfg --target x86_64-unknown-none`
reports `target_feature="fxsr"` and `target_feature="x87"` and **no `sse`, no
`sse2`** — `rust/compiler/rustc_target/src/spec/targets/x86_64_unknown_none.rs`
sets `RustcAbi::Softfloat` and `+soft-float`. That is load-bearing for the whole
design (§6.4) and is therefore asserted at build time (§8).

---

## 2. Defect 1 — a pending x87 exception kills the next unrelated process

Proven on CI, by one token, before this task started. `probe-x87.yml` run
`31260763462`: two arms, three reps each, one runner, one commit, one shard, and
the only difference is `fault_gate_child`'s x87 control word.

| arm | `fault_gates` | `std_unwind` | `std_unwind_so` |
|---|---|---|---|
| `control` (`cw = 0x037E`, IM unmasked) | PASS ×3 | **FAIL ×3** | **FAIL ×3** |
| `masked` (`cw = 0x037F`) | PASS ×3 | PASS ×3 | PASS ×3 |

`fault_gates` passing in both arms is what proves the probe is not vacuous.
Written up in full at `specs/issues/isolation/` and `specs/ci-plan.md` §9.3.

**The victim instruction is `FLDCW`**, a *waiting* x87 instruction: it checks for
a pending unmasked exception before it executes. It sits in the unwinder's
`restore_context`, and it is not exotic. Counted with `llvm-objdump` over the
six userland binaries this tree builds — `shell`, `doom`, `sshd`, `compositor`,
`toybox`, `terminal` — every one carries exactly:

- one `fnstcw`, in `unwinding::unwinder::arch::x86_64::save_context`;
- four `fldcw`, one in each of `__Unwind_RaiseException`,
  `__Unwind_ForcedUnwind`, `__Unwind_Resume_or_Rethrow` and `__Unwind_Resume`.

**Every ToyOS process executes one on every panic.** `std_unwind` is not
special; it is the only test that panics on a thread often enough to be caught.

---

## 3. Defect 2 — two Ring 3-reachable entries save no FP state at all

Not three copies of one save. Five shapes, eight binary copies of the save
sequence, and two shapes that save nothing:

| entry | saves XMM/MXCSR | can switch before returning to Ring 3 |
|---|---|---|
| `syscall_entry` (`arch/syscall.rs`) | for the handler, not the epilogue | yes |
| `timer_entry`, Ring 3 arm (`arch/idt/timer.rs`) | yes | yes |
| `device_irq_entry!` ×6 (`arch/idt/device_irq.rs`) | yes | yes |
| **`common_entry` — all 19 exception vectors, #PF included** (`arch/idt/mod.rs`) | **no** | **yes** |
| **`tlb_flush_entry`** (`arch/idt/tlb.rs`) | **no** | **yes** |
| `nmi_entry` (`arch/idt/nmi.rs`) | no | no — correct, and says so |

Both of the two say "yes" in the right-hand column in the code itself:
`common_entry` calls `kernel_exit_to_user_check` on the Ring 3 return path, and
so does `tlb_flush_entry`. That helper reaches `scheduler::do_preempt`. So **a
Ring 3 demand-paging fault can return to userland holding another thread's XMM
registers and MXCSR.** It corrupts data instead of faulting, which is why
nothing has noticed: an `MXCSR` carrying the wrong rounding mode or a stale
`xmm0` produces a wrong number, not a signal.

**A third instance, found while writing the bracket.** `syscall_entry` restored
the user state *before* calling `kernel_exit_to_user_check`, which reaches
`do_preempt`. A switch in that window returned to Ring 3 carrying whatever the
task that ran in between had left in the registers — the same defect through a
narrower window, on the busiest entry there is.

`arch/idt/tlb.rs` line 3 reads *"See xhci_entry for register-save rationale"* —
and then does not follow it. `xhci_entry` expands `device_irq_entry!`, which
parks XMM+MXCSR across the epilogue and documents why; `tlb_flush_entry` pushes
ten GPRs and calls the same epilogue with nothing parked. That is CLAUDE.md's
*a doc comment is a claim to verify* catching a live bug: the comment was the
only thing asserting the two agreed, and it was wrong.

---

## 4. The design

### 4.1 One bracket

`kernel/src/arch/entry.rs` exports `save_user_state!()` and
`restore_user_state!()`. They are the **only text in the kernel naming an FP
instruction**, and all five Ring 3-reachable shapes use them. "Saved some of it"
stops being expressible, because there is nothing to say it with.

### 4.2 A type owns the state

`kernel/src/arch/fpu.rs`:

```rust
#[repr(C, align(16))]
pub struct UserFpState([u8; 512]);
```

Exactly two constructors and one consumer:

- `saved_from_cpu()` — what the macro produces, and the only way to make one
  from live registers;
- `INITIAL` — a `const` FXSAVE image: `FCW = 0x037F`, `FTW = 0`,
  `MXCSR = 0x1F80`, everything else zero. This is the *declared* state the
  invariant's second clause names;
- the restore, which consumes one.

The size and alignment reach the assembly as `const { size_of::<UserFpState>() }`
and `const { align_of::<UserFpState>() }` operands, so a reservation cannot
disagree with the type. §5 is why that matters.

### 4.3 The IDT will not install anything else

`Ring3Entry` and `Ring0Entry` are the two things `IdtEntry` accepts, and its
raw-pointer constructor is private. Every `direct` row of `idt_vectors!` answers
which it is — the same column the error-code form already is, one level down —
with no third spelling and no default; `syscall::init` takes one too, because
`LSTAR` is an IDT slot by another name. Two rows are `ring0` and each says why:
the NMI arrives between arbitrary instructions, including inside another entry's
own save, and reschedules nothing; the halt IPI never returns.

**The type does not prove the bracket is present.** Nothing short of reading the
assembly does, and the doc says so rather than claiming more. What it makes
unrepresentable is installing a handler *without answering the question*, which
is exactly what `tlb_flush_entry` did for its whole life.

### 4.4 Not a `Drop` guard

The failing path is naked assembly that returns through `iretq`/`sysretq`, and a
task killed by another CPU never returns through it at all. CLAUDE.md's caveat
with teeth applies exactly: an RAII guard would bind the paths that already
work and miss the one that does not. The bracket is two macro invocations in the
same naked block, which is the strongest thing available where there is no Rust
scope to hang a destructor on.

---

## 5. AVX-512 is a stated future requirement

The owner has stated that the kernel must eventually support AVX-512.

**`FXSAVE64` is the interim, and it is complete for the machine as configured
today** (§1). Enabling any `XCR0` component beyond x87+SSE — AVX, AVX-512,
anything — **requires switching to XSAVE in the same commit**, because the
moment `XCR0` names a component, `FXSAVE64` stops being a complete save and
starts being a silent partial one, which is defect 2 again with a wider blast
radius.

That warning is worth nothing as prose, so it is not prose. The save area's size
and alignment come from `UserFpState` through `const {}` asm operands, and the
type's own definition carries the constraint. Growing the state means growing
the type, and growing the type moves every reservation with it — a mismatch is a
compile error, not a comment somebody did not read.

## 6. Decisions on the record

### 6.1 `FXSAVE64`/`FXRSTOR64`, unconditionally, no runtime branch

Not the cheap option — the *complete* one for this machine, and the only one
that exists on all three instruments. The dev host's guest has no XSAVE at all:
QEMU 11.0.3's `qemu64` model, which is what gets selected whenever KVM is
unusable, reports `fxsr: true, xsave: false, avx: false`. CI's KVM guest and the
T14 both have it. Choosing XSAVE would mean `cargo test` exercises a *different
kernel* from CI and metal on the one path this task exists to fix — the exact
blind spot that hid the AMD `SYSRET` bug (`specs/issues/kernel/`).

Two details that are not incidental:

- **REX.W forms.** Plain `FXSAVE` saves only the low 32 bits of `FIP`/`FDP`.
  `FXSAVE64` is the one that saves a 64-bit address.
- **`FXSAVE`/`FXRSTOR` are non-waiting.** They do not check for a pending
  unmasked exception, which is precisely what lets the kernel save a poisoned
  FPU without faulting in Ring 0. `FSAVE`/`FRSTOR` are waiting and would trap
  on entry — the wrong family for this job. This is why defect 1's fix cannot
  be "just `fninit` on entry" either: `FNINIT` discards the state instead of
  preserving it.

### 6.2 No runtime depth counter

The type gate in §4.3 makes the mistake uncompilable. Two extra instructions on
the syscall path forever, to re-check at run time something the compiler already
refuses, is not a trade worth making.

### 6.3 Eager, and lazy ruled out

Lazy FP restore — leave `CR0.TS` set, restore on the first `#NM` — is the
classic optimisation, and it is wrong here three times over:

1. It is **CVE-2018-3665** (LazyFP). Speculative execution reads the stale
   register file across the `#NM` boundary. Every major kernel removed lazy FPU
   switching over it.
2. It **buys ToyOS nothing.** The saving is for kernel threads that never touch
   FP state, and this kernel is soft-float: the only non-FP context is the idle
   loop, which does not context-switch through a Ring 3 entry.
3. It would **corrupt `#NM`'s meaning.** Vector 0x07 currently kills the
   process, which is correct — nothing legitimate raises it. Under lazy restore
   it would mean "the kernel deferred work", and the vector could no longer tell
   a kernel mechanism from a userland bug.

### 6.4 The state lives on the kernel stack

Per *ring transition*, not per task: a task can be inside several nested
transitions and each owes its own predecessor a restore. `context_switch`
already swaps kernel stacks, so the stack is the thing whose lifetime matches.
512 bytes on a 128 KiB stack (`KERNEL_STACK_SIZE`, `process.rs`) is free while a
transition is in flight, and costs nothing while one is not.

---

## 7. What S0 read off the machine, and the three divergences it found

Before any of this was built, one permanent per-CPU line was added to
`percpu::init` (`arch/fpu.rs::log_state`) naming `CR0`, `CR4`, `XCR0` when
`OSXSAVE` permits it, `CPUID.01H:ECX[26,27]`, and `CPUID.0DH` subleaves 0 and 1.
Nothing in the cost story is honest until the machine has been asked what state
it actually has.

Per CPU rather than once for the machine — unlike the SMEP/SMAP/PCID line beside
it — because "every CPU answers this identically" is the assumption under test,
not a reason to print less. The dev host's guest, `smp=2`, TCG:

```
[kernel 0.000 cpu0] fpu: cpu0 cr0=0x80010033 cr4=0x310668 xsave=0 osxsave=0 xcr0=0x0 cpuid.d.0=(0x0,0,0) cpuid.d.1.eax=0x0
[kernel 0.221 cpu1] fpu: cpu1 cr0=0xe0000011 cr4=0x310620 xsave=0 osxsave=0 xcr0=0x0 cpuid.d.0=(0x0,0,0) cpuid.d.1.eax=0x0
```

`xsave=0` and `osxsave=0` on both, so `FXSAVE64` is complete here and §6.1's
claim about the dev host holds.

**The two control registers do not match, and all three of the planner's
hypotheses are confirmed.** The AP trampoline (`arch/smp.rs`) sets `CR0.PE`,
`CR4.PAE`, `EFER.LME` and `CR0.PG` and touches nothing else, so an AP arrives in
long mode holding the INIT value of `CR0` with two bits or'd in — which is
exactly `0xe0000011`:

| bit | BSP | AP | consequence on an AP |
|---|---|---|---|
| `CR0.NE` (5) | 1 | **0** | an unmasked x87 exception takes the legacy FERR#/IGNNE path, not `#MF` |
| `CR0.WP` (16) | 1 | **0** | supervisor writes ignore the read-only bit |
| `CR0.CD` (30), `CR0.NW` (29) | 0 | **1** | **caching disabled** |
| `CR0.MP` (1) | 1 | **0** | `WAIT`/`FWAIT` no longer trap on `TS` |
| `CR4.MCE` (6) | 1 | **0** | a machine check is a shutdown, not `#MC` |
| `CR4.DE` (3) | 1 | **0** | debug-register access semantics differ |

`CR4.OSXSAVE` (18) is clear on both here, so hypothesis 1's *specific* worry —
firmware leaving `OSXSAVE` set so cpu0 permits AVX and the APs `#UD` on it —
cannot be answered on this host. But its *mechanism* is confirmed: the BSP's
`CR4` genuinely is firmware's and the APs' genuinely is built from zero, and
they already differ in two bits. On the T14 that question is open and only the
T14 can close it.

**`CR0.NE = 0` on every AP is a complete explanation for `specs/issues/isolation/`'s
unexplained survivor.** `fault_gates`' `mf` arm killed its child 6 of 6 alone
and survived once under a 12-wide suite, printing status word `0xb881` — IE set,
ES set, on the `fnstsw` two instructions past the `fwait` that should have
trapped on exactly that `ES`. "The state was not lost; the trap was." With `NE`
clear, an unmasked x87 exception is signalled through the external FERR# pin
instead of `#MF`, and nothing in a modern machine is listening. A child that
happened to be scheduled on an AP rather than the BSP would see precisely that.

**These are separate defects and are not fixed here.** They are recorded in
`specs/issues/` with this evidence. Note in particular that they
confound measurement: an AP with `CR0.CD` set runs with caching off, so any
microbenchmark that lands on one is not measuring what it thinks it is.

---

## 8. The kernel's soft-float promise becomes checkable

§1's soft-float guarantee is now load-bearing — it is the whole reason the FPU
may be left dirty between a save and its restore. So it is asserted where
`assert_overflow_checked` is, in `src/build.rs`: the kernel target must still
report no `sse` in its cfg. A future target-spec edit that turns hardware float
back on for the kernel stops the build rather than quietly making every bracket
in `entry.rs` insufficient.

---

## 9. Stages — all done

One commit each, each leaving `main` compiling. No ABI change, so no sysroot
claim.

- **S0** — read the machine. §7. Done first, because nothing below is honest
  until it has run, and it found three defects of its own.
- **S1** — `arch/fpu.rs`: `UserFpState`, `INITIAL`, `fpu::init()`,
  `fpu::self_check()` (`FNINIT` + `LDMXCSR(0x1F80)` + `FXSAVE64`, compared
  against `INITIAL` modulo `MXCSR_MASK`, per CPU).
- **S2** — `arch/entry.rs`; the three existing FP sites rewritten through it,
  still XMM+MXCSR only. It also closed defect 2's third instance
  (`syscall_entry`'s epilogue, §3).
- **S3** — bracket `common_entry` and `tlb_flush_entry`. **Defect 2.**
- **S4** — the bracket body becomes `fxsave64`/`fxrstor64`. One place.
  **Defect 1.**
- **S5** — the loader's trampolines load the declared state, so a task that has
  never been in Ring 3 starts from one. `FNINIT` would not do: it marks the x87
  registers empty without clearing them, so an `FXSAVE` reads the old data back
  out, and it does not touch XMM at all.
- **S6** — the gate (§10), and the correction that made its page-fault half
  real.
- **S7** — `Ring3Entry`/`Ring0Entry`, and the soft-float build assertion (§8).

S3 before S4 so the two cost components would be separately attributable. In the
event they are not, and §12 says why: the effect is smaller than this host's
run-to-run spread except in the six-pair interleaved A/B, which measures the
branch as a whole.

---

## 10. Gating

`fault_gates` is not touched and no `EXPECTED_FAILURES` entry is added.
`specs/ci-plan.md` §9.3 already ruled on both: giving `fault_gates` its own boot
would delete the only observation of the defect this tree has, and an exemption
would be one bought to make a run green while a process can kill its neighbour.

New permanent `fpu_isolation`, three halves, all positive assertions:

1. **Leak.** Child A pins a distinctive full FP state and exits without
   restoring it; child B asserts architectural defaults at entry. Fails on
   today's tree for XMM as well as x87.
2. **Fault.** Child A is `fault_gate_child mf`; child B executes `FLDCW` and
   must survive.
3. **Preservation.** One process pins a state, forces many transitions of each
   kind — 20,000 syscalls, two demand page faults, a preemption spin — against
   an FP-heavy sibling, and asserts bit-identity.

**`smp=1`, and that is the stronger machine rather than the weaker one.** Two of
the arms are about a register file carrying from one process to the next, which
needs the two to share a CPU; on two CPUs that is a coin flip, which is why CI's
own observation of the defect was intermittent.

**An unbracketed transition only corrupts if it switches**, and arm 3 arranges
that rather than hoping for it. Kernel code is soft-float, so a `#PF` that
allocates a page and returns disturbs nothing however unbracketed it is; what
does the damage is another task running Ring 3 code in between. The sibling
sleeps 100 µs between bursts, so several wakes land inside a single 2 MiB fault
— hundreds of microseconds, off the kernel's own fault trace — and
`need_resched` is set by the time `common_entry` reaches its exit.

**Negative control:** a `fpu-save-nothing` kernel feature under which
`fpu_isolation` must fail. Per CLAUDE.md the feature **replaces the behaviour,
never the verdict** — the reservation, the alignment, the `rsp` bookkeeping and
every assertion are the shipped ones, and what is absent is the `fxsave64`, the
`fxrstor64` and the trampoline's load. QEMU has no device, machine property or
`-cpu` flag that makes a guest's FPU carry over, so there is no other way to
have a control at all.

The driver collects every arm's verdict rather than stopping at the first,
because the control's job is to show that *each* arm has teeth. Measured on that
kernel, all seven fail and each says why:

```
FAILED: leak, round 0..2: a process started with the previous one's FP registers
FAILED: fault, round 0..2: FLDCW took an exception the process never caused
FAILED: preservation: the x87 control word did not survive … 0x0c7f became 0x037f
```

---

## 11. Cost

34 register-moving instructions become 2. That is a count of the existing
sequence and not a claim about time: `stmxcsr` + 16 `movdqu` on the way in and
16 `movdqu` + `ldmxcsr` on the way out, per bracket, times eight binary copies.
Instruction count falls; µop count and latency very plausibly rise, because
`FXSAVE64` is a microcoded 512-byte store.

**There is no measurement in this tree either way**, so this task produces one.

- **TCG is not evidence and will mislead.** QEMU implements `FXSAVE` as a helper
  call. The precedent points the same way: one `fetch_add` per log line cost
  **350 ms of boot** under TCG while being nearly free on hardware
  (`specs/issues/hardware/`). The TCG delta is recorded anyway and **labelled**, so
  nobody bisects a boot-time regression that is not one on real silicon.
- **A microbenchmark**: a userland loop of N `SYS_CLOCK` calls timed with
  `rdtsc`, reporting cycles per syscall (`tests/toyos-rust-tests/src/bin/`
  `syscall_cost.rs`; read it with `cargo test -- syscall_cost --nocapture`). It
  **prints, never asserts** — a TCG threshold is meaningless and a metal one
  drifts.
- CI's KVM guest is the second real instrument. **Metal is the verdict**, and
  that needs the owner.

**There is no page-fault arm, and the reason is a finding of its own.** The
first two attempts measured nothing: `sys_mmap` allocates and maps its whole
region up front, so a first touch of a fresh anonymous mapping is an ordinary
store, and `toyos-ld` writes `.bss` into the file, so the loader's `Anonymous`
tail is always empty. What is left is a writable file-backed page, at 2 MiB of
test image per fault — too expensive for a benchmark, which is why
`fpu_isolation` buys two and `syscall_cost` buys none. Both are recorded in
`specs/issues/hardware/`. The exception entry pays the same two instructions the syscall
arm measures.

---

## 12. Measurements

Every number here comes from a command that was run. All of it is **TCG on the
dev host**, where `FXSAVE` is a QEMU helper call: it is a labelled distortion
and not a claim about the T14 (CLAUDE.md's 1.06×–6.5× non-uniformity, and the
`fetch_add` that cost 350 ms of boot under TCG and nothing on silicon).

**Instruction count.** 34 register-moving instructions become 2, in each of the
eight binary copies the bracket replaced: `stmxcsr` + 16 `movdqu` in, 16
`movdqu` + `ldmxcsr` out, against `fxsave64` and `fxrstor64`.

**Cycles per `SYS_CLOCK`**, minimum over 9 repetitions of 20,000 calls,
`kernel/` checked out from the merge base for `before` and from this branch for
`after`, alternating, two sessions of three pairs:

| session | before | after |
|---|---|---|
| 1 | 870, 896, 907 | 993, 1007, 1016 |
| 2 | 891, 914, 956 | 1046, **1931**, 1093 |

Taking each arm's minimum over all six pairs: **870 before, 993 after — +123
cycles, +14%.** The bold outlier was taken at host load 14.94 with another
worktree's suite running; it is left in because dropping a number for being
inconvenient is how a measurement stops being one.

**What the same numbers say about the refactor.** S2 replaced eight copies of
the XMM+MXCSR sequence with one bracket and changed nothing about what is saved;
S4 changed the body. The before arm above *is* the pre-branch kernel, so the
+123 is S4's, and S2's own cost is inside the noise of a single arm.

**A boot-time figure is not offered.** The suite's own boot times move by more
between runs on this host than the whole effect.

---

## 13. Owed

`arch/entry.rs` and `arch/fpu.rs` are x86-64 and live in `arch/` beside every
other x86-64 file in this kernel. The `arch/x86_64/` split this tree owes is not
made here; it is a rename of a dozen files and belongs to whoever makes it for
all of them at once.
