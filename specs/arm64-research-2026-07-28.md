# ARM64 for ToyOS — High-Level Research

> 2026-07-28. Six parallel investigations, three competing designs, one synthesis.
> The HVF numbers are **measured on this M4 Pro**, not cited — the investigator built
> UEFI benchmark payloads from one C source and ran them under x86-64/TCG,
> aarch64/HVF and aarch64/TCG. Scope is deliberately high-level: enough to choose
> an approach, not to start typing.

## 0. Decisions taken (2026-07-28)

**Motivation: ARM64 support is the goal in itself**, not a dev-loop optimisation.
The measured HVF numbers below and the 635 MB initrd finding are still true and
still worth acting on, but they are *not* the reason and must not be treated as
an alternative. The report's "case against" argues partly from ergonomics ROI —
that argument does not apply. What survives of it is only the scheduling
collision: Stage 7c deletes `scheduler.rs` outright, including the exact
`context_switch` and idle handshake an abstraction-first port would refactor
first. Sequence around that, do not re-litigate whether to do it.

**Dispatch: compile-time, statically resolved.** One `cfg_attr(path)` module
selection. No `dyn Arch`, and no `Kernel<A: Arch>` type parameter — the first
buys indirect calls on the fault and lock paths, the second infects every static
in the kernel, and both model a decision the target triple already made.

Note the `const _: fn() -> CpuId = imp::cpu_id;` lines in §2 are **signature
assertions, not dispatch**. Call sites are direct calls to `imp::*`, inlinable
and zero-cost; the `const _` block exists so that a missing or mis-typed item
fails to compile at the contract file, naming the item, instead of at a call
site. `Hw` stays a trait only where a second implementation genuinely exists.

## 1. Is ARM64 faster on this host?

## Yes — but not as a flat multiplier, and the honest number is per-operation-class.

**Verified on this machine, this session:** `qemu-system-aarch64 -accel help` → `hvf, tcg`. `qemu-system-x86_64 -accel help` → `tcg` only. Same QEMU 11.0.2 install. Host: Apple M4 Pro, 14 cores, `kern.hv_support=1`. There is no path to accelerating the *existing* x86-64 target on this host — Hypervisor.framework virtualizes only ARM64 guests, and Rosetta translates user-space binaries, never kernels. Today's dev loop is unaccelerated TCG, confirmed in the tree: `src/qemu.rs:115-116` gates KVM on `cfg!(target_arch = "x86_64")`, which is false on this host, so `src/qemu.rs:5` launches `qemu-system-x86_64` with no `-accel` at all.

**What gets faster (dossier HVF microbenchmarks, ARM64/HVF vs x86-64/TCG, min-of-N):**

| Operation | Speedup | Absolute |
|---|---|---|
| Address-space switch + TLB flush | **6.5x** | 251 ns vs 1619 ns |
| Random memory read-modify-write (4 MiB set) | **3.4x** | 596.7 vs 176.0 Miter/s |
| Atomics at smp≥2 | **~1.4x** | (at `-smp 1` TCG elides real atomicity — the smp≥2 figure is the honest one, and `tests/common/qemu.rs` runs `-smp 8`) |
| Dependency-bound ALU loop | **1.06x** | TCG's best case — a latency-bound xorshift chain hides translation overhead behind the M4's pipeline |

**What gets *worse*:** device-register access is a **5.2x regression** — 876 ns per MMIO exit under HVF vs 168 ns for TCG's in-process device model. HVF does not merely fail to accelerate device I/O; it is actively worse, because every MMIO touch is a real VM exit while TCG handles it in-process.

**Why that profile is the argument.** CLAUDE.md's own known-issues list names context-switch volume under TCG as the main obstacle to full-speed single-core Doom ("~half the CPU is unattributed kernel time… expensive under TCG"). Address-space switching is exactly where HVF wins biggest, and driver MMIO — where it loses — is a small fraction of steady-state cycles. Expect the loop to feel substantially faster for scheduler-heavy and userspace-compute work, and slightly worse for driver bring-up.

**The boot-time expectation must be tempered, and this is the single most useful number in the dossier.** A real ToyOS boot is **4.96 s of host wall-clock** to reach "Boot: complete" — not the 387 ms the guest reports (that timestamp comes from a TSC that TCG calibrates against a skewed clock and does not measure what a developer waits). Of the 4.96 s: **2.48 s is loading the initrd** over emulated USB, **1.42 s is OVMF firmware**, and only **0.93 s is the ToyOS kernel**. I verified the size independently: `target/bootable.img` is 646 MB (`ls -la`, built 2026-07-28 14:13), consistent with the ~635 MB initrd loaded at `bootloader/src/main.rs:365`. **Half your boot wait is architecture-independent and fixable today.** ARM64+HVF plausibly takes 4.96 s to roughly 1–3 s, but that is unverified extrapolation and the largest single component of it is a fix that needs no ARM64 at all.

**What is lost:**

- **`-d int` exception/interrupt logging.** Gone under HVF — hardware takes exceptions directly and QEMU never sees them. Measured: same guest, `-d int,cpu_reset` produced 1,544 bytes under HVF (CPU-reset dump only) vs 126,975 bytes of interrupt traces under TCG. This is a direct regression against the debugging workflow CLAUDE.md documents.
- **The guest PMU.** `ID_AA64DFR0_EL1.PMUVer = 0` under HVF, = 6 under aarch64 TCG. This bites Layer 3 (RIP sampling) on the diagnostics roadmap.
- **EL2 / nested virtualization.** The M4 hardware and macOS support it (`hv_vm_config_get_el2_supported → 1`), but QEMU 11.0.2 refuses: `-machine virt,virtualization=on` fails under HVF. No VHE, no nested virt for the guest.
- **Not lost: gdb.** Full parity under HVF — software breakpoints, hardware breakpoints, all three watchpoint types, single-step, `-action shutdown=pause`, QMP. Byte-for-byte the same GDB-RSP verdicts as TCG.

**The mitigation that makes the loss acceptable:** one `qemu-system-aarch64` binary supports both accelerators. Bring ARM64 up under `-accel tcg` (keeps `-d int` and the PMU during the phase that most needs exception traces), then switch the default to `hvf` and keep TCG as a first-class debug configuration. That leaves you strictly better off than today, where x86-64 has no accelerated option at all — and it retires two standing CLAUDE.md known issues, because PCID/ASID and absolute-deadline-timer paths become testable on real hardware semantics for the first time.

## 2. Recommended approach

**Take the mechanism from `module`, the interface-shape decisions from `module` and `prior-art`, the trait exception (narrowed hard) from `trait`, and the epistemics and Act-0 sequencing from `portfirst` — and add one thing none of them proposed.** Concretely: select the architecture with a single `cfg_attr(path)` on one module, declare the contract as typed function-pointer bindings in that one file so a missing or mis-typed item is a compile error naming the item, and **do not** introduce a kernel-wide `Arch` trait or type parameter — the measured surface is 43 `crate::arch::` references resolving to ~20 distinct symbols dominated by `percpu::cpu_id`, called from the fault handler and every lock path, so `dyn Arch` buys indirect calls on the hottest paths and `Kernel<A: Arch>` infects every static, to model a decision the target triple already made. Keep `Hw` a trait exactly where it is, because its second implementation genuinely exists on the host; treat an MMU trait as *deferred with a named trigger*, not built up front. Then spend the entire complexity budget where the two prior Rust kernels actually failed — on interface **shape**: make TLB invalidation an *implementation* obligation rather than a caller obligation, make memory type a mandatory parameter of every mapping, and adopt ARM's model (TTBR0/TTBR1 roots, mandatory ASID, absolute-deadline timer) as the portable one wherever ARM is the stricter architecture, letting x86 collapse it. The addition: encode the invalidation obligation in the *type system* with a `#[must_use]` token that only `map`/`unmap` can construct and only `invalidate` can consume — which makes "changed a translation and forgot to tell the TLB" unrepresentable rather than reviewed, and is the project's own `unrepresentable > checked > tested` rule applied to the exact bug Redox and Theseus both shipped.

```rust
// kernel/src/arch/mod.rs — the ENTIRE architecture selection and contract.
// One page. Every addition to it gets argued. That is the enforcement.

#[cfg_attr(target_arch = "x86_64",  path = "x86_64/mod.rs")]
#[cfg_attr(target_arch = "aarch64", path = "aarch64/mod.rs")]
mod imp;

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!("ToyOS supports x86_64 and aarch64 only");

pub use imp::{IrqGuard, PerCpuExt, TableRoot};

/// Memory type. On ARM64 this is the MAIR index and it is the ONLY source of
/// the type. On x86-64 today it is supplied by firmware MTRRs and the kernel
/// has no concept of it at all: `mm/paging.rs:552` maps device BARs with
/// `PAGE_PRESENT | PAGE_WRITE`, and `mm/paging.rs:645-666` blankets every
/// physical address up to `max_addr` with those same flags — a Normal-cacheable
/// alias over every MMIO hole. Making this a parameter is a bug fix on x86 and
/// a hard prerequisite on ARM64.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MemAttr {
    Normal,        // writeback cacheable — RAM
    Device,        // nGnRnE — MMIO registers
    NormalNoCache, // uncached, reorderable — DMA buffers
}

/// Proof that a translation changed and the TLB has not been told yet.
/// Constructed only by `map`/`unmap`; consumed only by `invalidate`. No Drop,
/// no Default, no constructor — so "unmap without invalidate" does not compile.
/// Ranges merge, so a mapping loop still pays exactly one invalidate.
///
/// This is the whole defence against the failure Redox shipped live
/// (`src/arch/aarch64/ipi.rs` = `// FIXME implement` empty bodies while generic
/// code spins on an ack that never arrives) and Theseus shipped independently
/// (an aarch64 IPI handler for a shootdown ARM64 does not need). Both froze an
/// x86-shaped *caller* obligation into portable code. A trait would not have
/// caught either: an empty trait impl compiles exactly as well as an empty fn.
#[must_use = "a translation change that is never invalidated is a stale-TLB bug"]
pub struct Invalidate { range: VirtRange, asid: Asid }

impl Invalidate {
    pub fn merge(self, other: Invalidate) -> Invalidate { /* widen range */ }
}

// The contract. A missing or mis-typed item fails HERE, naming the item, on
// whichever architecture is being built — no generics, no indirection, no
// type parameter anywhere else in the kernel.
const _: fn() -> CpuId                      = imp::cpu_id;
const _: fn() -> IrqGuard                   = imp::irq_guard;
const _: fn(CpuId)                          = imp::kick;
const _: fn() -> Nanos                      = imp::now;
const _: fn(Nanos)                          = imp::set_timer;      // ABSOLUTE deadline
const _: fn(&TableRoot, Asid)               = imp::activate_user;  // TTBR0 only
const _: fn(&mut TableRoot, VirtAddr, PhysAddr, Perm, MemAttr) -> Invalidate = imp::map;
const _: fn(&mut TableRoot, VirtAddr) -> Invalidate                          = imp::unmap;
const _: fn(Invalidate)                     = imp::invalidate;     // ARM: TLBI ...IS.
                                                                   // x86: invlpg + IPI, PRIVATELY.
const _: fn(CpuId, u8) -> MsiTarget         = imp::msi_target;     // kills the three
                                                                   // hardcoded 0xFEE0_0000
```

Three properties of that snippet carry the whole design. `invalidate` takes a token instead of the kernel calling `crate::arch::apic::tlb_shootdown()` — so the IPI lives *inside* x86's implementation and simply does not exist on ARM64, and the interface has no concept the better architecture does not need. `activate_user` takes only the user root — so ARM64 gets TTBR0/TTBR1 from hardware and x86 expresses the same invariant by installing the kernel half once, deleting the `PML4[256..512]` copy in every `new_user` at `mm/paging.rs:249-253`. And `set_timer` takes an absolute `Nanos` — which is `CNTP_CVAL_EL0` semantics natively on ARM64 and forces x86 onto `IA32_TSC_DEADLINE`, closing a listed known issue. Note this is *the same shape `toyos-sched/src/hw.rs:88-93` already has* — I verified `fn now(&self) -> Nanos` and `fn set_timer(&self, deadline: Nanos)` in the tree. The design is not new; it is the existing boundary generalized without generalizing its *mechanism*.

## 3. Why

**Simplicity, and the >2x rule (CLAUDE.md).** A trait must buy more than 2x over a module to justify itself. For portability alone it buys nothing: exactly one architecture is ever linked into a given kernel binary, so the trait's dispatch is resolved at build time either way. What a trait *does* buy is a second implementation on the host — and that is a real, large win, which is why `Hw` should stay a trait. But the win belongs to the specific subsystem that gets a simulator, not to the kernel. The `trait` proposal's own framing concedes this ("portability is the by-product; the host-executable test story is the reason"), and then generalizes anyway to a Tier-2 kernel-wide `Arch` trait for which no second implementation is proposed. That fails its own test.

**The measured surface says the same thing.** I counted it: `grep -rn 'crate::arch::' kernel/src | grep -v '^kernel/src/arch/'` → **43 references**, resolving to **~20 distinct symbols**: `apic::{arm_one_shot, ensure_armed_before, eoi, halt_all_cpus, kick_cpu, stop_timer, tlb_shootdown}`, `cpu::{halt, invlpg, invpcid, outw, rdfsbase, read_cr3, write_cr3}`, `idt::kernel_exit_to_user_check`, `percpu::current_tid`, `smp::{apic_id_for, cpu_count}`. This is wide and shallow, dominated by per-CPU identity queries in the lock and fault paths. Threading `A: Arch` through it, or paying an indirect call for `cpu_id()`, is complexity with no compensating return.

**The prior-art evidence points at shape, not mechanism.** Hubris — the plainest mechanism found, a 31-line `arch.rs` with `cfg_attr(path)` and no trait — has **zero** `target_arch` in its generic kernel across `syscalls.rs` (945), `task.rs` (924), `kipc.rs` (579), `umem.rs` (439). Theseus — the most aggressive mechanism, crate-per-arch with 36 target-cfg dependency blocks and a typed `PteFlags` abstraction — leaked **210** `target_arch` occurrences across 49 files. Redox, in between, leaked 59 across 27.7k generic lines. Containment is anti-correlated with mechanism sophistication. And the failure that actually damaged both Redox and Theseus — freezing an x86-shaped IPI+ack TLB shootdown into portable code, then shipping a dead aarch64 path for it (`redox/src/arch/aarch64/ipi.rs` empty `// FIXME implement` bodies while `src/percpu.rs:77-115` spins on `while ackword.load() < affected_cpu_count`) — is invisible to trait-vs-module. `trait Ipi { fn ipi(&self); }` with an empty impl compiles exactly as well. The `module` stance is right that spending the budget on mechanism buys protection against a failure nobody has hit.

**But `module`'s own defence is too weak, and this is where I go past all three.** Its contract file admits `const _: fn(PhysRange, Asid) = imp::flush;` is satisfied by an empty body — it says so honestly, and falls back on "human review of a one-page file." CLAUDE.md and the project's own memory say `unrepresentable > checked > tested`. The `#[must_use] Invalidate` token *is* the unrepresentable version: the invalidation obligation is carried in a value that only the mapping operations mint and only the flush consumes, so the Redox failure — a caller obligation the arch cannot honor — cannot be expressed at all. None of the three designers proposed this. It costs about 20 lines.

**Zero technical debt (CLAUDE.md) rules out `portfirst`'s Act I.** It proposes carrying ~4,200 lines of x86-64 in duplicate, of which its own ledger admits 1,200–1,500 are semantically parallel enough to need mirrored fixes, in the hottest and most correctness-critical 18% of the kernel, until an Act II extraction that may not happen. The `portfirst` author surfaces the decisive counter-evidence himself: `specs/scheduler-core-spec.md:800` — which I verified — says *"Zero-legacy principle: we do not carry a parallel old world one stage longer than the conversion requires."* A judge-reviewed spec in this repository already decided this trade for a different subsystem, deliberately, against duplication. And the predictable failure is the worst kind: a memory-ordering fix lands in the x86 copy and not the ARM64 one, and ARM64 is the weaker memory model, so the copy that most needs the fix is the one that misses it.

**What I *do* take from `portfirst`, and it is substantial.** Its epistemic point is the sharpest single observation in the entire dossier and I verified it: `grep -rn "impl Hw"` across the whole tree returns **exactly one result** — `toyos-sched/sim/src/hw_impl.rs:111` (`SimHw`). `KernelHw` is Stage 6 of 10 and has never been written. ToyOS's flagship pre-designed hardware boundary has never been implemented by actual hardware, on the architecture it was extracted from. Proposing five more such boundaries for hardware nobody in this project has touched, before the first clears Stage 6, is compounding an unsettled bet. That is why my recommendation defers the MMU trait rather than building it, and why the plan opens with three cheap experiments to convert the dossier's three `[unverified]` facts into evidence *before* the interfaces that depend on them are designed. "Always be empirical… Never guess at root causes" is a CLAUDE.md workflow rule, and it applies to interface design too.

**Rust is first class, fail fast.** `compile_error!` on an unknown target (Hubris's pattern) plus the typed contract bindings plus a CI grep banning `todo!`/`unimplemented!`/empty bodies under `arch/aarch64/` gives the fail-fast behaviour the project demands: an incomplete architecture screams and dies at build time rather than compiling into a lying stub.

**Development ergonomics above all — but read the measurement honestly.** The prize is real and verified on this host (`hvf` for aarch64, `tcg` only for x86-64). It is also narrower than "emulation gets faster": 6.5x on address-space switch, 1.06x on ALU, and a 5.2x *regression* on MMIO. And half the current boot wait is a 635 MB initrd load that needs no architecture work. The ergonomics principle therefore argues first for the initrd fix, second for host-testable coverage of `kernel/src/sync.rs` (which loom does not currently see — `toyos-sched/loom/Cargo.toml` compiles only `toyos-sched/src/*.rs`), and only third for the port.

## 4. What it deletes

## (a) What ARM64 makes unnecessary — never written for the second architecture

All line counts verified in the tree this session unless marked estimate.

| Mechanism | ARM64 replacement | Lines not written |
|---|---|---|
| Real-mode AP trampoline at phys 0x8000, temp 32-bit GDT, PM32→LM64 far jumps, INIT-SIPI-SIPI with retry (`arch/smp.rs:44-183, :221-235, :282-384`) + the reserved trampoline page (`main.rs:233`) | PSCI `CPU_ON`: one SMC, secondary arrives at 64-bit EL1 | **~240 of smp.rs's 384** |
| TLB shootdown IPI: `arch/idt/tlb.rs` (verified **53 lines**, whole file), `Vector::TlbFlush = 0xFE`, `apic::ipi_all_excluding_self(0xFE)` | Broadcast `TLBI …IS` — hardware, no IPI exists. FEAT_TLBIRANGE confirmed present on this M4 (`ID_AA64ISAR0_EL1.TLB = 2`) | **~70** |
| PCID allocator + CPUID-gated fallback (`mm/paging.rs:111-213`, `arch/cpu.rs:221-273`) | 16-bit ASID in the top bits of TTBR0_EL1 — architectural, always present, one register write, no fallback branch. `ASIDBits = 2` measured | **~180** |
| GDT, TSS, IST, and the dual `kernel_rsp` + `tss.rsp0` write on every switch (`arch/percpu.rs:18-45, :98-170, :283-288`) | No segmentation; banked `SP_EL1` | **~110** |
| 8259 PIC disable, 24 lines of `outb` to 0x20/0x21/0xA0/0xA1 (`arch/idt/mod.rs:289-312`) | Does not exist | **24** |
| Two calibration loops: TSC-vs-HPET (`clock.rs:26-44`), LAPIC-vs-HPET (`arch/apic.rs:112-134`), plus HPET discovery (`main.rs:255-257`) | `CNTFRQ_EL0` reports the frequency directly; `CNTPCT_EL0` is a fixed-frequency monotonic counter | **~50** |
| Tick conversion + `last_armed_ticks` + two raw `wrmsr 0x838` re-arm sites in ISR asm + `ensure_armed_before` race workaround (`apic.rs:140-166`, `percpu.rs:88-95`, `idt/timer.rs:55-59, :92-96`) | `CNTP_CVAL_EL0` is a 64-bit absolute compare — TSC-deadline semantics as the default, not a CPU feature | **~45** |
| Kernel-half PML4 copy into every new address space (`mm/paging.rs:249-253` — verified: `for i in 256..512`) | TTBR1_EL1 programmed once at boot; context switch writes only TTBR0_EL1 | **5**, and the invariant becomes architectural |
| The ring-3 runtime check repeated in 6 naked-asm shapes across 11 IDT vectors (`idt/mod.rs:207`, `timer.rs:11`, `msix.rs:44`, `tlb.rs:29`) | VBAR_EL1's 16 slots split by source-EL × {Sync, IRQ, FIQ, SError} — "came from userspace" is *which slot you are in* | structural, not counted |
| `panic_raw_uart` port I/O, `inb(0x3FD)`/`outb(0x3F8)` (`main.rs:91-99`) | AArch64 has no port I/O address space at all | ~10 |

**ARM64 backend estimated at ~2,500–3,000 lines** (dossier estimate, unverified) against the x86-64 backend's 4,297 in `arch/` plus ~1,700 misfiled elsewhere. The second architecture costs roughly 60% of the first.

## (b) What x86-64 should be restructured or deleted to match

**Genuinely deleted from x86-64 — no ARM64 required, each landable alone:**

| Change | Deletes | Independent justification |
|---|---|---|
| Make PCID **mandatory**, not CPUID-gated | ~**180** lines: the fallback branches at `mm/paging.rs:134-139, :147-149, :174-178` and the probe at `arch/cpu.rs:221-273` | CLAUDE.md already records this path as untestable under TCG. Cost: drops pre-2010 x86 — which the zero-legacy rule drops anyway |
| Move the timer to `IA32_TSC_DEADLINE` | ~**45** lines: ns→ticks math, `last_armed_ticks`/`armed_deadline_ns` fields, both raw `wrmsr 0x838` re-arm sites in ISR asm, `ensure_armed_before` | Closes a listed CLAUDE.md known issue. Also deletes the LAPIC-vs-HPET calibration, ~**23** more |
| Delete `disable_pic` | **24** lines (`arch/idt/mod.rs:289-312`) | Pure legacy on a UEFI-only, x2APIC-only kernel. Zero-legacy already disowns it; it just has not been executed |
| Install the kernel half once (TTBR-shaped roots) | **5** lines (`mm/paging.rs:249-253`) | Turns "user pointer in a kernel table" from *checked* (`mm/mod.rs:25-27`) into *unrepresentable* |
| Replace the 15 `offset_of!` asserts (`arch/percpu.rs:175-189`) and the `gs:[N]` literals re-hardcoded across `preempt.rs` (7 sites), `log.rs:27-28`, `idt/msix.rs`, `idt/timer.rs`, `idt/tlb.rs`, `idt/mod.rs` | ~**15** lines of asserts, plus a whole hand-policed hazard class | Removes a compile-time-assert block whose only job is guarding a design that should not need guarding |

**Verified x86-64 deletion subtotal: ~290 lines.** Add against that what the design *adds* to x86: `MemAttr` plumbing plus a hole-aware direct map (~60–100), and the contract file (~40–60). Netting the build-system collapse (38 hardcoded triples across `src/build.rs`, `src/toolchain.rs`, `src/image.rs`, `src/libc.rs` → one `Arch` value) as roughly neutral-to-negative, the **x86-64 side comes out at roughly −150 to −250 lines net, plus a real correctness fix.**

**Restructured, not deleted — the interface-level simplifications, which matter more than the line count:**

- **Six caller-obligation sites lose the obligation.** `mm/paging.rs:209, :556, :570` and `arch/syscall.rs:1039, :1377` currently call `crate::arch::apic::tlb_shootdown()` after `invlpg`. I confirmed `map_mmio` at `mm/paging.rs:547-558` does exactly this. Under the `Invalidate` token the IPI moves *inside* x86's implementation, where it also unblocks the targeted per-page shootdown already on the known-issues list — because the interface finally carries an address range instead of "send an IPI".
- **`MemAttr` on every mapping is an x86 bug fix, not ARM64 scaffolding.** Verified: `map_mmio` maps device BARs with `PAGE_PRESENT | PAGE_WRITE` (`mm/paging.rs:552`) and `init` blankets every physical address from 0 to `max_addr` with the same two flags (`mm/paging.rs:664`), which necessarily covers MMIO holes. There is no PAT/MTRR/PCD/PWT code anywhere in the kernel. It works today only because firmware MTRRs force UC over the PCI hole. That is a latent x86 bug that survives review precisely because nothing forces the question.
- **~1,520 lines leave `arch/`.** `kernel/src/arch/syscall.rs` is 1,658 lines; I confirmed the arch-specific part is `init` at :24 plus the naked `syscall_entry` at :38-133 — about 110 lines. Everything from `syscall_dispatch` (:134) through `kill_process` (:1656) — `sys_write`, `sys_mmap`, `sys_dlopen`, `sys_io_uring_*`, 50+ handlers — is portable kernel logic misfiled under arch. **This is a move, not a deletion**, but it shrinks the architecture surface by more than the entire ARM64 port adds to it.
- **~1,700 lines move *into* `arch/`.** `mm/paging.rs`'s x86 half (of 765), `preempt.rs`, `clock.rs` init, `rtc.rs`, `drivers/serial.rs`'s 16550 port I/O, the `elf.rs` `R_X86_64_*` dispatch. I verified 7 non-arch files still contain `asm!`: `log.rs`, `scheduler.rs`, `main.rs`, `preempt.rs`, `loader.rs`, `drivers/log_ring.rs`, `drivers/serial.rs`. `arch/` is 18.1% of the kernel (4,297 / 23,725) while real coupling is ~25%. That gap *is* the first refactor.
- **Three hardcoded `0xFEE0_0000` LAPIC MSI addresses in device drivers** (`virtio_net.rs:178`, `virtio_sound.rs:423`, `xhci/mod.rs:424`) collapse into one `msi_target(cpu, vector)`.

## The honest bottom line, since you asked for it plainly

**Per architecture, the kernel gets smaller and substantially clearer. In total, it grows.** There is no honest way to make a two-architecture kernel smaller than a one-architecture kernel, and I am not going to invent deletions to claim otherwise. What is genuinely available:

1. **~290 lines leave x86-64 permanently** and never come back, most of them mechanisms the project already knows it cannot test.
2. **~1,520 lines stop pretending to be architecture code**, which is the largest correctness-of-layout fix available in the tree and needs no ARM64.
3. **The second backend costs ~60% of the first** (~2,500–3,000 vs 4,297+1,700), because ARM64 needs none of the trampoline, GDT/TSS, PIC, PCID, or IPI-shootdown machinery.
4. **One real x86 bug is fixed** (the Normal-cacheable direct-map alias over MMIO) that only an ARM64-shaped interface forces you to see.

Net kernel size after a completed port: roughly **+2,300 to +2,800 lines**, against a 23,725-line kernel. The compensating claim is not size, it is that both halves become simpler than the single half is today, and that two known issues and one latent bug close on the way.

## 5. Page granule

## Keep "2 MiB only". Choose the 4 KiB translation granule on ARM64 and map with L2 block descriptors.

**This is settled by measurement, not inference.** A bare-metal AArch64 probe on this actual M4 Pro under HVF read `ID_AA64MMFR0_EL1 = 0x000010000F100022`: TGran4 = supported, TGran16 = supported, **TGran64 = not supported**. Cross-checked against QEMU CPU models: `-cpu max` supports all three, `cortex-a72` lacks 16 KiB, `neoverse-n1` supports all three. **The 4 KiB granule is the only granule supported by every implementation tested.** Under it, an L2 block descriptor maps exactly 2 MiB.

**The structural match is exact and I verified the ToyOS side.** `kernel/src/mm/paging.rs:92-99` extracts indices at shifts 39/30/21 — bit-identical to ARM64's 4 KiB-granule L0=VA[47:39], L1=VA[38:30], L2=VA[29:21]. The `#[repr(C, align(4096))] [u64; 512]` page-table page is already the correct ARM64 4 KiB-granule table. The 4-level ARM walk collapses to 3 levels exactly as x86-64's does. **`indices()` needs no change for either architecture** — only the descriptor *encoding* differs, and that is behind the arch boundary by construction.

**The host's 16 KiB page size does not constrain the guest.** `hw.pagesize` is 16384 on this Mac (verified), but the guest's stage-1 granule is `TCR_EL1.TG0/TG1` and the hypervisor's stage-2 is `VTCR_EL2.TG0` — architecturally independent. Apple's 16 KiB preference binds the DART IOMMU on bare metal, not a guest. Asahi Linux runs 4 KiB-page kernels inside VMs on this same hardware for exactly this reason.

## The cost, stated plainly

**Internal fragmentation, and it is bad.** ToyOS allocates a whole 2 MiB page per (thread, TLS module) — `arch/syscall.rs:1471`, `loader.rs:72-73` — so a typical ~1 KiB TLS block wastes 99.95%. 64 threads × 1 module = 128 MiB. Worse, `pmm::alloc_page` memsets the entire 2 MiB on every allocation (`mm/pmm.rs:249-253`), so handing out a page for a 1 KiB block costs a 2 MiB zeroing pass. Against that, 2 MiB pages cut demand-fault counts by 128–512x — the 8 MiB user stack takes 4 faults instead of 2,048 — which matters a great deal under TCG.

**Do not fix fragmentation by changing the page size.** Fix it by not using one-page-per-allocation for sub-page objects: suballocate TLS blocks and the other small `PageAlloc` users out of a page. That helps x86-64 today, helps ARM64 identically, and keeps the one-page-size rule intact.

**The per-arch PAGE constant is the expensive option, and it does not boot.** I verified both of the load-bearing sites. `mm/alloc.rs:12` uses `PAGE_2M` as the kernel heap's **maximum allocation size** (`assert!(size <= PAGE_2M as usize, "GlobalAlloc: dlmalloc asked for {} bytes")`), and kernel stacks are 128 KiB — a 16 KiB PAGE panics on the first thread spawn. `mm/pmm.rs:148` sizes the PMM bitmap as a fixed `MAX_PAGES = 32768` array covering exactly 64 GiB at 2 MiB; at 16 KiB that is 512 MiB and at 4 KiB it is 128 MiB, so `pmm.rs:217`'s boot assert fires against the 2 GiB the dev loop uses and the 4 GiB the test harness uses. Of 137 lines across 27 files coupled to the choice, ~115 are mechanical and survive a rename — but the ~20 that are not are exactly the ones that turn "a simple constant change" into a multi-week debugging exercise.

## One prerequisite refactor worth doing regardless

`PAGE_2M` currently means four different things: page size, kernel-heap maximum allocation, PMM bitmap scaling constant, and — across the ABI — the virtio DMA region size, which `userland/netd` and `userland/soundd` each hardcode as the literal `2 * 1024 * 1024` on the other side of the syscall boundary. Rename to `PAGE` and split the conflated meanings into `MAX_HEAP_ALLOC`, `PIPE_CAPACITY`, `DMA_REGION_SIZE`, `GUARD_SIZE`. Mechanical, testable, x86-only, and it makes the constant mean one thing. This belongs in P0/P2, and it is worth doing even if the granule never changes — which, per the above, it should not.

**One ARM64 fact that does change:** the guest sees a **40-bit physical address space** under HVF (`PARange = 2`), narrower than x86-64 typically assumes. Any code assuming a wider PA is a portability hazard. 40 bits is 1 TiB, comfortably above the PMM's 64 GiB ceiling, so nothing breaks today — but it should be an explicit constant rather than an assumption.

## 6. Sequencing sketch

Seven phases. Every checkpoint is a green x86-64 build under the existing gates (`cargo test`, plus `tests/audio-baseline.toml` — which I confirmed is currently all-clean/strict for `audio_tone` and `audio_tone_load` at both smp1 and smp8).

**P0 — Baseline and unblock. No architecture work whatsoever.**
Fix the initrd load (`bootloader/src/main.rs:365`; `target/bootable.img` measured at 646 MB). It is 2.48 s of a 4.96 s boot, it multiplies across every test in the suite, it costs zero ARM64 design risk, and doing it first is what lets you attribute later wins. Collapse the 38 hardcoded triples plus the `qemu-system-x86_64` call into one `Arch` value carrying {toyos triple, none triple, uefi triple, qemu binary, accelerator}. Give `toyos-ld` and `toyos-cc` their first tests — both have **zero** `#[test]`, and `toyos-ld` already contains 18 applied `R_AARCH64_*` relocation types plus aarch64 PLT synthesis that **have never executed**, because the Mach-O emitter that motivated them is reachable only by hand from `main.rs:181`. A linker that silently mis-patches an ADRP immediate produces a program that runs and computes the wrong address; that is the worst bug class this project could take on.
*Checkpoint: measurably faster boot, x86 green, one build-system arch parameter, golden-object linker tests.*

**P1 — Convert the three unverified facts into evidence. Days, not weeks. Zero kernel changes.**
(1) Compile a `#[thread_local]` probe for `aarch64-unknown-none` and read which TLS relocation family LLVM actually emits. This is the dossier's one flagged `[unverified]` toolchain fact and it decides whether `toyos-ld` needs 4 TLS relocation types or 20, and whether the portable TLS model is TLSDESC-shaped or `__tls_get_addr`-shaped. (2) Execute an `ICC_SGI1R_EL1` access under `-accel hvf -machine virt,gic-version=3`. `ID_AA64PFR0_EL1.GIC` reads 0 under HVF regardless of `gic-version`, and nobody has executed an `ICC_*` access — the interrupt-controller interface must not be locked in on top of an unexecuted instruction. (3) Boot an edk2 ARM64 UEFI payload under HVF and measure to first output, to size the real prize rather than extrapolating.
*Checkpoint: three facts on paper. If (2) fails, the IRQ design changes before it is written, not after.*

**P2 — Make `arch/` honest. x86-only, pure relocation, no new interface.**
Introduce `kernel/src/arch/mod.rs` as the selection + contract file (today it is 10 lines of flat `pub mod` with zero cfg — verified). Evacuate ~1,520 lines of `sys_*` from `arch/syscall.rs` to `kernel/src/syscall/`. Move `mm/paging.rs`'s x86 half, `preempt.rs`, `clock.rs` init, `rtc.rs`, `serial.rs`'s port I/O, and `elf.rs`'s reloc dispatch into `arch/x86_64/`. **Explicitly out of scope: `scheduler.rs`'s naked `context_switch` and `cli/sti;hlt` idle handshake, and `loader.rs`'s `iretq` stubs.** See the case-against — spec §11 Stage 7c deletes `scheduler.rs` entirely; refactoring it now is work scheduled for deletion.
*Checkpoint: no `asm!` outside `arch/` except the scheduler/loader sites deliberately left to the migration. Nothing deleted yet, nothing behind a new interface yet.*

**P3 — Interface shape. x86-only. Each item independently justified and independently landable.**
`MemAttr` on every mapping plus a hole-aware direct map (fixes the real bug). The `Invalidate` token, making invalidation an implementation obligation and pulling the IPI inside x86's impl. TTBR-shaped roots: kernel half installed once, `activate_user` takes only the user root. Mandatory ASID: delete the PCID fallback. Absolute-deadline `set_timer` on `IA32_TSC_DEADLINE`: delete the tick math and the re-arm-from-asm hack. `PerCpu` with an arch extension field replacing the 15 asserted `gs:[N]` offsets.
*Checkpoint: this is where the ~290 lines leave and the MMIO attribute bug closes. Each item gates on the audio + scheduler harnesses separately, so any one can be reverted alone.*

**P4 — Make the boundary real before there is a second architecture.**
`cargo check --target aarch64-unknown-none` enters the green loop. `arch/aarch64/` is created and grows item by item; a CI grep bans `todo!`, `unimplemented!`, and empty-bodied functions inside it. **This is the non-negotiable.** Both Redox's dead `ipi.rs` and Theseus's dead aarch64 shootdown survived review precisely because nothing forced generic code to justify itself against a second architecture. A boundary only one architecture ever compiles is not a boundary.

**P5 — Toolchain.** `aarch64_unknown_toyos.rs` target spec (the x86 one is 24 lines and `base::toyos::opts()` is already arch-independent; std's ToyOS pal has zero `target_arch` gates). `toyos-ld` ELF output arm — `emit_elf.rs:1021`'s literal `e_machine: EM_X86_64` plus 19 `R_X86_64_*` emission sites and a 12-byte `adrp/ldr/br` PLT stub. TLS model chosen from P1's evidence, unified rather than duplicated — this is the one place I agree with `portfirst`'s rule: duplicate what *hardware* dictates, unify what the *toolchain* dictates, because TLS crosses four repositories and forking it compounds outward. std needs two asm files: `_start` (4 instructions) and `__tls_get_addr` (~30), out of 3,121 ToyOS-specific lines. 13 of 14 ecosystem forks need nothing.

**P6 — Bring-up under `-accel tcg` first.** One binary serves both accelerators, and TCG keeps `-d int` and the guest PMU during exactly the phase that needs exception traces. Order: UEFI handoff (note: the AArch64 UEFI binding hands over with the **MMU enabled** and UEFI-map RAM identity-mapped — structurally unlike x86, and the most likely place to stall), MMU, PL011, generic timer, VBAR_EL1 exceptions, first EL0 entry, then GICv3, then PSCI SMP, then drivers.

**P7 — Flip the default to `hvf`; keep TCG first-class for tracing.** Re-baseline the audio gate per (arch, accelerator) — see the missed-items section; the existing histograms are not comparable across accelerators.

**Where the always-green checkpoints really fall:** P0, P2, and each individual item within P3 are all independently revertible and independently valuable. P1 changes no code at all. Everything through P4 leaves x86-64 strictly better whether or not ARM64 ever lands — that is the hedge, and it should be a deliberate one.

## 7. The case against doing this now

## The strongest argument is scheduling collision, and it is concrete.

**The scheduler migration is mid-flight and it plans to delete the code this port wants to refactor.** I read the spec's stage table. Stage 7 is sub-staged: **7a** rewrites the percpu `CpuSched`, the driver idle loop, the asm switch and the trampoline; **7c** *deletes `scheduler.rs` entirely*, along with `handle_outgoing`, `park_outgoing`, `wake_by_event`/`EventSource`, `drain_events`, `PERCPU_EVENTS`, `IN_SCHEDULE`, `POISONED`, `KILLED`, `CpuQueueGuard::into_raw`, `Lock::force_unlock`, the `loader.rs` trampoline unlock, and the global blocked pool. Every abstraction-first proposal in this dossier puts `scheduler.rs`'s `context_switch` and `cli/sti;hlt` idle handshake, and `loader.rs`'s `iretq` stubs, into its early x86 refactor stages. **That is work a judge-reviewed, half-executed migration is scheduled to delete.** My P2 explicitly carves those out, but the carve-out is itself an admission: a meaningful slice of the "clean up x86 first" value is unavailable until Stage 7c lands.

**There are ten specs in `specs/`, and at least four describe unbanked migrations that collide with this one.** `capability-handles-spec.md` (Fd→Handle, refcounted objects behind typed per-process handles) churns exactly the ~1,520 lines of `sys_*` that P2 wants to relocate — and a 1,520-line file move is the single worst thing you can do to an in-flight branch touching those functions. `iouring-blocking-spec.md` rewrites blocking I/O, which touches the same wake sites Stage 5 is converting right now. `memory-ownership-spec.md` and `allocation-owners.md` touch the memory management that P3 restructures. Adding an eleventh track when four are unbanked is how a project ends up with five 40%-complete migrations and no shippable state.

**The ergonomics prize is narrower than the headline.** "ARM64 under HVF is faster" is true but non-uniform: **6.5x** on address-space switch, **3.4x** on memory, **~1.4x** on real atomics, **1.06x** on a dependency-bound ALU loop, and a **5.2x regression** on device MMIO. And half of the boot wait a developer actually experiences — 2.48 s of 4.96 s — is a 635 MB initrd load that is architecture-independent and fixable this week. If the motivation is "the dev loop is too slow," the highest-value action is P0, and P0 needs no ARM64. If, after P0, the loop feels acceptable, the case for the port weakens considerably.

**The project's flagship pre-designed hardware boundary has never been implemented by hardware.** I verified this: `grep -rn "impl Hw"` across the entire tree returns exactly one result, `toyos-sched/sim/src/hw_impl.rs:111` (`SimHw`). `KernelHw` is Stage 6 of 10 and has not been written. `Hw` is repeatedly cited — by two of the three designers and by CLAUDE.md — as proof that ToyOS can design a clean hardware boundary in advance. It is currently proof that ToyOS can design one and *not yet find out whether it was right*. Committing to a second, third and fourth such boundary before the first clears Stage 6 is compounding an unsettled bet, and the honest response is to let Stage 6 land first — it is only two stages away and it will tell you, cheaply, whether the `Hw` shape survives contact with LAPIC/ICR/asm.

**HVF costs you debugging tools during exactly the phase you need them.** `-d int` is gone (measured: 1,544 bytes vs 126,975 under TCG) and the guest PMU is gone. The TCG-first mitigation works, but it means the ergonomics win arrives *last*, after the bring-up phase where it would have been most motivating.

**The `Invalidate` token is my own contribution and it has a failure mode.** A `#[must_use]` value that must be threaded from `map`/`unmap` to `invalidate` is viral in the same way a lifetime is. Any call site that maps in a loop, or maps behind an abstraction that returns something else, has to plumb it. If it turns out to need an escape hatch — a `fn forget(self)` for the boot-time direct-map construction, say — that hatch immediately reintroduces the failure it was designed to prevent, and now with the false confidence of a type that looks like it enforces something. It should be prototyped on the x86-only P3 stage and abandoned without ceremony if it fights the mapping code.

## What I would actually tell the architect

Do **P0 and P1 now** — they are cheap, unconditional, and P1 converts three guesses into facts before any interface depends on them. Fold the P2/P3 items into the *scheduler migration's own stages* where they overlap, rather than running a parallel track. And **gate the real ARM64 commitment on the scheduler migration reaching Stage 7c**, at which point `scheduler.rs` is gone, `KernelHw` exists, and you will know from evidence rather than from architecture review whether the `Hw` boundary shape was correct — which is precisely the question this entire port is a large bet on.

If ARM64 is not going to happen — if it is an interesting idea rather than a commitment — say so now, because P2 and P3 are a substantial refactor whose remaining justification without ARM64 is one real bug fix, two closed known issues, and ~290 deleted lines. That is genuinely worth doing on its own merits, but it is a much smaller claim than the one this report opens with, and it should be chosen deliberately rather than arrived at by abandonment.

## 8. What the designers missed

**1. Nobody proposed making the invalidation obligation unrepresentable.** All three correctly identified the Redox/Theseus failure — an x86-shaped IPI+ack TLB shootdown frozen into portable code, with a lying aarch64 stub — and all three proposed the same defence: name the interface `flush(range, asid)` instead of `tlb_shootdown()`, and review it carefully. The `module` stance explicitly concedes its contract file accepts an empty body, and falls back on "human review of a one-page file." But CLAUDE.md and the project's own memory say `unrepresentable > checked > tested`. A `#[must_use]` token that only `map`/`unmap` can mint and only `invalidate` can consume makes the bug not compile. It costs ~20 lines. This is the single largest gap in all three proposals.

**2. `Hw` has exactly one implementor.** Verified: `grep -rn "impl Hw"` returns only `toyos-sched/sim/src/hw_impl.rs:111`. The `trait` and `module` stances both lean on `Hw` as the validated in-house precedent for a hardware boundary. It has never been implemented by hardware — `KernelHw` is Stage 6, unwritten. `portfirst` caught this and it is the sharpest observation in the dossier; the other two missed it while citing the same crate as their strongest evidence.

**3. The audio gate is not comparable across accelerators, and nobody raised it.** I read `tests/audio-baseline.toml`: it records per-(test, smp) underrun histograms, currently all-clean/strict, and stages 1–6 of the scheduler migration gate on no regression against them. Audio is period-rate *device I/O* — precisely the operation class where HVF is a measured **5.2x regression** (876 ns per MMIO exit vs 168 ns). An ARM64/HVF audio histogram cannot be compared against an x86/TCG baseline, and an ARM64/TCG one is a third distinct configuration. The always-green story depends on this gate, so the baseline file needs an (arch, accelerator) dimension and three sets of recorded histograms — a real, unbudgeted piece of test-harness work that all three sequencing plans assume away.

**4. The RNG is worse than "needs an arch branch" — it has no design at all.** I read it: `sys_random` (`arch/syscall.rs:579-591`) is a bare loop calling `cpu::rdrand()` (`arch/cpu.rs:33`, a `rdrand`/`jnc` spin) and copying the result straight into the user buffer. There is no entropy pool, no reseeding, no mixing. Under HVF, `ID_AA64ISAR0_EL1.RNDR = 0` — **no hardware RNG at all on this host**. So ARM64 does not need a different instruction; it needs an entropy source and a pool that do not currently exist, which is a kernel security design decision. All three lenses listed the RNDR=0 fact; none of the three *proposals* costed or sequenced the work.

**5. `MAX_CPUS = 8` versus the 128-core goal, and the GICv3 target-list constraint.** Verified at `scheduler.rs:18`, with hardcoded 8-element array literals (`CPU_TIME_NS`, `IN_SCHEDULE`, and `trace.rs:95-98`). Separately, `ICC_SGI1R_EL1`'s TargetList is 16 bits *per affinity cluster*, so a targeted kick is one write but any broadcast-to-a-set must iterate affinity groups. Both facts appear in the dossier; no proposal sequenced them, and the second is a genuine design constraint on the IPI interface at 128 cores.

**6. `try_lock`'s missing acquire edge may be on code that is scheduled for deletion.** All three flagged it, correctly: I verified `sync.rs:50-52` loads `now` Relaxed and CASes `ticket` with Acquire, while the release is `now.fetch_add(1, Ordering::Release)` at the guard's `Drop` — a *different* atomic, so no synchronizes-with edge exists. It guards the cross-CPU steal that hands a kernel stack pointer between CPUs. But spec §11 Stage 7c deletes the steal-and-blocked-pool machinery outright and replaces it with message-passing wakes. Nobody connected the two. The right move is to check whether Stage 7 removes the site entirely before fixing it in place — and if it does not, that is itself important information about the migration.

**7. One overclaim worth naming.** The `trait` stance asserts "I expect the kernel to be smaller after the x86-only stages than it is today, before any ARM64 code exists." That is not supportable. The 1,520-line evacuation from `arch/syscall.rs` is a *move*; `MemAttr` plumbing, a hole-aware direct map, and the contract file all *add* code. The honest x86 net is roughly −150 to −250 lines. Given that the owner is specifically worried the codebase only grows, an inflated deletion claim is worse than a modest true one.

**8. The AArch64 UEFI handoff differs structurally, and two of three proposals filed it under "unabstractable, use a postcondition."** The UEFI AArch64 binding requires the firmware to hand over with the **MMU enabled** and all UEFI-map RAM identity-mapped; x86 hands over with paging in whatever state the loader left it. Only `BRINGUP`'s facts flagged this. Both the `trait` and `module` proposals put boot in a tier where the type system contributes nothing — which is correct as far as it goes, but they did not note that the *state* being handed over is itself architecture-divergent, which is where a port most plausibly stalls and where an unexamined assumption is most expensive.

**9. A small piece of good news nobody banked.** `KernelArgs` (`toyos-abi/src/boot.rs`) is a flat `#[repr(C)]` struct of physical addresses whose one x86-specific field is `boot_pml4_addr` — and that field exists *solely* to feed the AP trampoline, which PSCI deletes. So the boot protocol is not "nearly portable"; it becomes **fully** portable, and the struct gets smaller. Related and less good: the kernel's `_start` decodes `KernelArgs` by hardcoded byte offsets in naked asm (`main.rs:157-173`) with no `offset_of!` assert, unlike `percpu.rs` — a silent ABI/assembly coupling that should be fixed in P2 regardless of ARM64.
