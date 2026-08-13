# The user machine state

**The invariant, which nobody had written down:**

> A transition out of Ring 3 that can reach another task must save and restore
> **the whole** user machine state, and a task that has never been in Ring 3
> must start from a **declared** state.

This document is what that sentence means on x86-64 as this kernel configures
the machine: the two defects the kernel shipped with before it held, and the
design that makes the class unrepresentable rather than fixed twice.

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
| XMM0–15, `MXCSR` | yes | the bracket (§4.1) saves it on all five Ring 3-reachable entries — §3 has the two that used not to |
| x87 registers, `FCW`, `FSW`, `FTW`, `FIP`, `FDP`, `FOP` | yes | the bracket body is `fxsave64`/`fxrstor64` (§9) — §2 has the entry that used to save neither |
| YMM/ZMM, opmask | not yet | `XCR0` is 1, so they do not exist — §5 |

`XCR0` is 1 because `CR4.OSXSAVE` is set nowhere: neither `kernel/src` nor
`bootloader/src` contains an `xsetbv`, and the one `xgetbv` (`arch/fpu.rs`,
gated `if osxsave`) is read-only diagnostics that never executes on this
machine. So the machine runs with the reset value of `XCR0`, which permits
x87 and SSE and nothing else. AVX `#UD`s. Therefore **`FXSAVE64`/`FXRSTOR64`
is a *complete* save of everything this kernel permits to exist**, not a
cheap approximation of one.

The kernel itself contributes nothing to that state: it is soft-float by
compiler guarantee. `rustc +toyos --print cfg --target x86_64-unknown-none`
reports `target_feature="fxsr"` and `target_feature="x87"` and **no `sse`, no
`sse2`** — `rust/compiler/rustc_target/src/spec/targets/x86_64_unknown_none.rs`
sets `RustcAbi::Softfloat` and `+soft-float`. That is load-bearing for the whole
design (§6.4) and is therefore asserted at build time (§8).

---

## 2. Defect 1 — a pending x87 exception kills the next unrelated process

Provable by toggling one bit: boot two children differing only in
`fault_gate_child`'s x87 control word (masked vs. IM unmasked), and only the
unmasked arm loses `std_unwind`/`std_unwind_so` while `fault_gates` itself
passes in both — proof the probe exercises the path rather than passing
vacuously. `specs/assessments/ci-plan-assessment-2026-08.md` §9.3 has the run.

**The victim instruction is `FLDCW`**, a *waiting* x87 instruction: it checks
for a pending unmasked exception before it executes. It is not exotic —
`unwinding::unwinder::arch::x86_64::save_context` executes one `fnstcw`, and
`restore_context`'s `fldcw`, reached from each of `__Unwind_RaiseException`,
`__Unwind_ForcedUnwind`, `__Unwind_Resume_or_Rethrow` and `__Unwind_Resume`,
is in every ToyOS binary that links the unwinder.

**Every ToyOS process executes one on every panic.** `std_unwind` is not
special; it is the only test that panics on a thread often enough to be caught.

---

## 3. Defect 2 — two Ring 3-reachable entries save no FP state at all

Five shapes reach Ring 3, and every one but `nmi_entry` can switch to a
different task before getting there: `syscall_entry`, `timer_entry`'s Ring 3
arm, `device_irq_entry!`'s six copies, `common_entry` (all 19 exception
vectors, #PF included) and `tlb_flush_entry`. Before the bracket in §4, two of
the five saved nothing: `common_entry` and `tlb_flush_entry` both call
`kernel_exit_to_user_check` on the Ring 3 return path, which reaches
`scheduler::do_preempt`, so **a Ring 3 demand-paging fault could return to
userland holding another thread's XMM registers and MXCSR.** It corrupted
data instead of faulting, which is why nothing had noticed: an `MXCSR`
carrying the wrong rounding mode or a stale `xmm0` produces a wrong number,
not a signal.

**A third instance surfaced while writing the bracket.** `syscall_entry`
restored the user state *before* calling `kernel_exit_to_user_check`, which
reaches `do_preempt`. A switch in that window returned to Ring 3 carrying
whatever the task that ran in between had left in the registers — the same
defect through a narrower window, on the busiest entry there is.

`tlb_flush_entry`'s own comment used to point at `xhci_entry` for its
register-save rationale and then not follow it: `xhci_entry` expands
`device_irq_entry!`, which parks XMM+MXCSR across the epilogue and documents
why; `tlb_flush_entry` pushed ten GPRs and called the same epilogue with
nothing parked. That is CLAUDE.md's *a doc comment is a claim to verify*
catching a live bug — the comment was the only thing asserting the two
agreed, and it was wrong.

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
  invariant's second clause names, and the loader's trampolines load it so a
  task that has never been in Ring 3 starts from it. `FNINIT` would not do:
  it marks the x87 registers empty without clearing them, so an `FXSAVE`
  reads the old data back out, and it does not touch XMM at all;
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

## 7. Every CPU agrees, and `log_state` is why anyone can say so

`percpu::init` logs each CPU's FPU-relevant state (`arch/fpu.rs::log_state`):
`XSAVE`, `OSXSAVE`, `XCR0` when `OSXSAVE` permits reading it, and the relevant
`CPUID` leaves. Per CPU rather than once for the machine, because "every CPU
answers this identically" is an assumption to check, not a reason to print
less. `XSAVE` and `OSXSAVE` read 0 on every CPU on every instrument this
kernel runs on — which is what makes §1's completeness claim hold across the
whole machine, not just the boot CPU.

Whether an AP's *control registers* agreed with the BSP's was a separate
question this line's first readings raised — `CR0` and `CR4` used to be part
of this same log line. Root `CLAUDE.md`'s "CPU state" section and
`specs/issues/kernel/ap-control-registers-inherit-init.md` have that story and
what it still owes; the fact load-bearing here is that `CR4.OSXSAVE` is
cleared by the same whole-register declaration that closed it
(`arch/control_regs.rs`), so no CPU can hold the bit this section exists to
rule out, and `CR0`/`CR4` moved to that file's own log line beside this one.

---

## 8. The kernel's soft-float promise becomes checkable

§1's soft-float guarantee is now load-bearing — it is the whole reason the FPU
may be left dirty between a save and its restore. So it is asserted where
`assert_overflow_checked` is, in `src/build.rs`: the kernel target must still
report no `sse` in its cfg. A future target-spec edit that turns hardware float
back on for the kernel stops the build rather than quietly making every bracket
in `entry.rs` insufficient.

---

## 9. Current state

The design in §4 is fully built: `UserFpState` and its self-check (§4.2),
`common_entry` and `tlb_flush_entry` bracketed along with the other three
Ring 3-reachable entries (closing defect 2, §3), the bracket body on
`fxsave64`/`fxrstor64` (closing defect 1, §2), the loader's trampolines
loading the declared state, `Ring3Entry`/`Ring0Entry` as the IDT's only
accepted shapes (§4.3), the gate (§10), and the soft-float build assertion
(§8). See git log for the incremental history.

---

## 10. Gating

`fault_gates` is not touched here and no `EXPECTED_FAILURES` entry is added.
`specs/assessments/ci-plan-assessment-2026-08.md` §9.3 already ruled on both: giving `fault_gates` its own boot
would delete the only observation of the defect this tree has, and an exemption
would be one bought to make a run green while a process can kill its neighbour.
Its `mf` arm became `Expect::Killed` once `CR0.NE` was declared on every CPU,
and it still shares that boot, which is the half the ruling protects.

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

34 register-moving instructions become 2 — a count of the existing sequence,
not a claim about time: `stmxcsr` + 16 `movdqu` on the way in and 16 `movdqu`
+ `ldmxcsr` on the way out, per bracket, replaced by `fxsave64`/`fxrstor64`.
Instruction count falls; µop count and latency very plausibly rise, because
`FXSAVE64` is a microcoded 512-byte store. §12 has the one measurement this
tree has taken of it.

- **TCG is not evidence and will mislead.** QEMU implements `FXSAVE` as a
  helper call. The precedent points the same way: one `fetch_add` per log
  line cost **350 ms of boot** under TCG while being nearly free on hardware
  (`specs/issues/hardware/`). A TCG delta is recorded anyway and **labelled**,
  so nobody bisects a boot-time regression that is not one on real silicon.
- **A microbenchmark**: a userland loop of N `SYS_CLOCK` calls timed with
  `rdtsc`, reporting cycles per syscall (`tests/toyos-rust-tests/src/bin/`
  `syscall_cost.rs`; read it with `cargo test -- syscall_cost --nocapture`). It
  **prints, never asserts** — a TCG threshold is meaningless and a metal one
  drifts.
- CI's KVM guest is the second real instrument. **Metal is the verdict**, and
  that needs the owner.

**There is no page-fault arm.** `sys_mmap` allocates and maps its whole
region up front, so a first touch of a fresh anonymous mapping is an ordinary
store, and `toyos-ld` writes `.bss` into the file, so the loader's `Anonymous`
tail is always empty. What is left is a writable file-backed page, at 2 MiB of
test image per fault — too expensive for a benchmark, which is why
`fpu_isolation` buys two transitions and `syscall_cost` buys none. Both are
recorded in `specs/issues/hardware/`. The exception entry pays the same two
instructions the syscall arm measures.

---

## 12. Measurements

Every number here comes from a command that was run, on **TCG on the dev
host** — `FXSAVE` is a QEMU helper call there, so this is a labelled
distortion, not a claim about the T14 (CLAUDE.md's 1.06×–6.5×
non-uniformity). **It also predates the AP control-register fix** (§7): both
arms below ran with three of four cores caching-disabled, so this is a
measurement of a machine that no longer exists, and by how much is unmeasured
(`specs/issues/kernel/ap-control-registers-inherit-init.md`).

Cycles per `SYS_CLOCK`, minimum across six interleaved before/after pairs
(`kernel/` at the merge base vs. this branch): **870 before, 993 after — +123
cycles, +14%.** The refactor that reorganised the save sites (§4.1) changed
nothing about what was saved; the switch to `fxsave64`/`fxrstor64` (§2, §9)
changed the body, so the +123 is attributable to that change alone.

A boot-time figure is not offered: the suite's own boot times move by more
between runs on this host than the whole effect.

---

## 13. Owed

`arch/entry.rs` and `arch/fpu.rs` are x86-64 and live in `arch/` beside every
other x86-64 file in this kernel. The `arch/x86_64/` split this tree owes is not
made here; it is a rename of a dozen files and belongs to whoever makes it for
all of them at once.
