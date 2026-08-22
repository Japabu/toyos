//! What `CR0`, `CR4` and `IA32_EFER` hold on every CPU in this machine.
//!
//! One declaration, applied by the BSP and by every AP, and checked on each of
//! them afterwards. Before this file there was no declaration at all: the BSP
//! ran with whatever firmware had left and an AP ran with the `INIT` value plus
//! the two bits `smp.rs`'s trampoline has to OR in to reach long mode, so cores
//! 1..N booted with **caching disabled**, `WP` clear and `NE` clear for the
//! whole history of the tree.
//!
//! All three are written whole, so every bit of each is decided here. `CR0`'s
//! value is a constant; four of `CR4`'s bits are the silicon's to offer, so its
//! value is *required plus whatever of the optional set this CPU has* — a
//! function every CPU evaluates and has to agree on. `EFER` is a constant, and
//! it is the register [`Prot`](crate::mm::paging::Prot) rests on: with `NXE`
//! clear, bit 63 of a paging entry is a *reserved* bit rather than a permission,
//! so a CPU that reached Ring 3 without it would either fault on every mapping
//! this kernel writes or — before this file owned the bit — run every one of
//! them as executable, and no test downstream could tell which.

use core::sync::atomic::{AtomicU64, Ordering};

use super::cpu;
use crate::log;

/// `IA32_EFER`, SDM Vol. 3A §2.2.1. Address from Vol. 4 Table 2-2.
mod efer {
    pub const MSR: u32 = 0xC000_0080;
    pub const SCE: u64 = 1 << 0;
    pub const LME: u64 = 1 << 8;
    /// Set by the CPU when paging turns long mode on, and read-only: a write
    /// cannot change it, so the check below reads it out of the comparison
    /// rather than declaring a value for it.
    pub const LMA: u64 = 1 << 10;
    pub const NXE: u64 = 1 << 11;
}

/// `CR0`, SDM Vol. 3A §2.5.
mod cr0 {
    pub const PE: u64 = 1 << 0;
    pub const MP: u64 = 1 << 1;
    pub const ET: u64 = 1 << 4;
    pub const NE: u64 = 1 << 5;
    pub const WP: u64 = 1 << 16;
    pub const NW: u64 = 1 << 29;
    pub const CD: u64 = 1 << 30;
    pub const PG: u64 = 1 << 31;
}

/// `CR4`, SDM Vol. 3A §2.5.
mod cr4 {
    pub const DE: u64 = 1 << 3;
    pub const PAE: u64 = 1 << 5;
    pub const MCE: u64 = 1 << 6;
    pub const OSFXSR: u64 = 1 << 9;
    pub const OSXMMEXCPT: u64 = 1 << 10;
    pub const LA57: u64 = 1 << 12;
    pub const UMIP: u64 = 1 << 11;
    pub const FSGSBASE: u64 = 1 << 16;
    pub const PCIDE: u64 = 1 << 17;
    pub const SMEP: u64 = 1 << 20;
    pub const SMAP: u64 = 1 << 21;
}

/// `CR0` on every CPU running this kernel.
///
/// The bits left out are as much of the declaration as the bits in it:
///
/// - `EM` (2) clear and `MP` (1) set — the pair that says an x87 instruction
///   executes on the FPU rather than raising `#NM` (SDM Vol. 3A §2.5), and that
///   `WAIT` respects `TS`.
/// - `TS` (3) clear — lazy FP switching is ruled out, because deferring the
///   restore behind `#NM` leaks the previous task's register file across the
///   deferral boundary. Nothing ever sets it and `#NM` keeps its meaning of
///   "a userland bug".
/// - `AM` (18) clear — with it set, `RFLAGS.AC` would make an unaligned Ring 3
///   access `#AC`. Nothing in this kernel is ready to be the thing that decides
///   a process wanted that.
/// - `NW` (29) and `CD` (30) clear — caching on, which is the defect this file
///   was written for.
pub const CR0: u64 = cr0::PE | cr0::MP | cr0::ET | cr0::NE | cr0::WP | cr0::PG;

/// The `CR4` bits every CPU must have, or the kernel does not run on it.
///
/// `PAE` is long mode's, `OSFXSR` and `OSXMMEXCPT` are `FXSAVE64`'s and SSE's,
/// and `MCE` is a machine check reported rather than a shutdown with nothing to
/// read. **`DE` is here for zero legacy and not for a need**: this kernel
/// references no `DR4` or `DR5` — `arch::debug` and the `#DB` handler use `DR0`,
/// `DR6` and `DR7` — and `DE` clear is the 386 behaviour where `DR4` and `DR5`
/// alias `DR6` and `DR7` instead of raising `#UD` (SDM Vol. 3B §17.2.2,
/// *Debug Registers DR4 and DR5*). All five are older than x86-64 and
/// present on everything that implements it, and `FSGSBASE` is not — but every
/// context switch uses `rdfsbase`/`wrfsbase`, so a CPU without it would `#UD` at
/// the first one. All are checked against CPUID rather than assumed: setting a
/// `CR4` bit the CPU does not define is `#GP`, and on the BSP that happens
/// before `idt::init`, where it is a triple fault with no report.
const CR4_REQUIRED: u64 = cr4::DE
    | cr4::PAE
    | cr4::MCE
    | cr4::OSFXSR
    | cr4::OSXMMEXCPT
    | cr4::FSGSBASE;

/// The `CR4` bits this kernel takes when the CPU offers them and does without
/// when it does not.
///
/// `UMIP` (bit 11) joined `SMEP`/`SMAP`/`PCIDE` here rather than
/// [`CR4_REQUIRED`] for the same reason they did: it is silicon's to offer,
/// not every CPU this kernel targets does, and `declaration` already checks
/// it against CPUID before setting it. With it set, `SGDT`, `SIDT`, `SLDT`,
/// `SMSW` and `STR` executed in Ring 3 raise `#GP` instead of handing a
/// process the GDT, IDT and TSS addresses (SDM Vol. 3A §2.5) — the addresses
/// a KASLR bypass is built out of, and nothing in this kernel's userland
/// executes any of the five. Free hardening, taken.
const CR4_OPTIONAL: u64 = cr4::SMEP | cr4::SMAP | cr4::PCIDE | cr4::UMIP;

/// `IA32_EFER` on every CPU running this kernel.
///
/// Three bits, and none of them optional:
///
/// - `LME` (8) — long mode, which this CPU is already in. Named rather than
///   preserved, because the register is written whole and dropping it here is
///   a `#GP` with paging on (SDM Vol. 3A §4.1.2).
/// - `NXE` (11) — bit 63 of a paging entry means *not executable* instead of
///   *reserved*. Every data mapping this kernel writes carries it
///   (`mm::paging::Prot`), and it was set nowhere until W^X existed.
/// - `SCE` (0) — `SYSCALL`/`SYSRET`. It used to be a read-modify-write in
///   `arch::syscall::init`, which is the shape this file exists to delete: two
///   places decided what one register held, and the second one could not see
///   the first.
///
/// `LMA` (10) is the CPU's and appears nowhere: it is read-only, so a write
/// with it clear leaves it set and a comparison against it would fail on every
/// CPU.
pub const EFER: u64 = efer::SCE | efer::LME | efer::NXE;

/// The declaration as the BSP computed it, for every AP to reproduce and match.
///
/// Zero until the BSP has been through [`init`], which is also what
/// [`pcid_active`] reads before then — the right answer, because no page has
/// been mapped and no TLB entry flushed at that point either.
static DECLARED_CR4: AtomicU64 = AtomicU64::new(0);

/// Put this CPU's `CR0` into [`CR0`].
///
/// The first thing each CPU does. On an AP that means *before*
/// [`pat::init`](super::pat::init), which brackets its MSR write with a no-fill
/// window and puts back the `CR0` it found — so an AP that reached it first
/// would carry `INIT`'s `CD` straight through the one sequence in the kernel
/// that could have cleared it.
pub fn init_cr0(cpu_id: u32) {
    let before = bench::sample();
    if !skipped(cpu_id) {
        let live = cpu::read_cr0();
        if live & (cr0::CD | cr0::NW) != 0 {
            // SDM Vol. 3A §11.5.3's first two steps — no-fill (`CD` set, `NW`
            // clear), then write back and invalidate — which is the order for
            // crossing between cached and uncached in either direction. `INIT`
            // leaves `CD` and `NW` both set, §11.5.1's mode where memory
            // coherency is *not* maintained, so a line this AP's caches held
            // from before is otherwise served to a CPU that has just been told
            // it is.
            // SAFETY: `write_cr0` asks the caller to own the whole machine
            // configuration, and this file *is* that owner — the value here is
            // the CPU's own live `CR0` with `CD` set and `NW` cleared, so no
            // other bit moves. `wbinvd` asks to be inside a no-fill window,
            // which the write on the line above has just opened; that ordering
            // is SDM Vol. 3A §11.5.3 and is the whole point of the pair.
            unsafe {
                cpu::write_cr0((live | cr0::CD) & !cr0::NW);
                cpu::wbinvd();
            }
        }
        // SAFETY: `write_cr0`'s contract, discharged by the declaration itself —
        // `CR0` is a constant this file computes for every CPU in the machine,
        // and its doc comment argues every bit in it and every bit left out.
        unsafe { cpu::write_cr0(CR0) };
    }
    bench::report(cpu_id, before);
}

/// Put this CPU's `CR4` and `EFER` into the declaration, then check that this
/// CPU holds all three registers the declaration names.
///
/// Later than [`init_cr0`] because `SMEP`, `SMAP` and `NXE` are statements
/// about the address space: they are set once the CPU is on the kernel's own
/// page tables, not on the bootloader's. `NXE` in particular reinterprets bit
/// 63 of every live paging entry, and the tables this kernel writes are the
/// ones it is a statement about.
///
/// Before `arch::syscall::init` on both paths — `percpu::init_bsp` runs it from
/// `main`, `percpu::init_ap` from `ap_entry`, and each is ahead of that call —
/// which is what lets `SCE` live in the declaration instead of in a
/// read-modify-write there.
pub fn init(cpu_id: u32) {
    let declared = declaration(cpu_id);
    if !skipped(cpu_id) {
        // SAFETY: `write_cr4` is `#GP` on a bit this CPU does not define, on
        // clearing `PAE`/`LA57` in long mode, and on raising `PCIDE` while
        // `CR3[11:0]` is non-zero. `declaration` has just asked CPUID for every
        // optional bit and asserted `LA57` is clear; `PAE` is in
        // `CR4_REQUIRED`; and both call sites of this function run on the kernel
        // address space, whose PCID is 0.
        unsafe { cpu::write_cr4(declared) };
        cpu::wrmsr(efer::MSR, EFER);
        if declared & cr4::SMAP != 0 {
            // SMAP binds only while `RFLAGS.AC` is clear, and `AC` here is
            // whatever was inherited — `INIT` clears it on an AP, firmware
            // answers for the BSP. Nothing in this kernel ever sets it: user
            // memory is reached by page walk and the direct map (`user_ptr`),
            // which SMAP does not cover, so there is no `stac`/`clac` pair
            // anywhere and clearing it once here is the whole protocol.
            cpu::clac();
        }
    }
    self_check(cpu_id, declared);
}

/// Whether the declaration has `PCIDE` in it, and therefore whether `INVPCID`
/// is the flush this machine uses.
pub fn pcid_active() -> bool {
    DECLARED_CR4.load(Ordering::Acquire) & cr4::PCIDE != 0
}

/// What this CPU says [`CR4_REQUIRED`] and [`CR4_OPTIONAL`] come to, checked
/// against what the BSP said.
///
/// Recomputed on every CPU rather than read off the BSP's answer, so a machine
/// whose cores do not offer the same features is a line naming the CPU instead
/// of a `#GP` in `ap_entry` with nothing to say.
fn declaration(cpu_id: u32) -> u64 {
    let have = supported();
    let missing = CR4_REQUIRED & !have;
    assert!(
        missing == 0,
        "control_regs: cpu{cpu_id} lacks CR4 bits {missing:#x} that this kernel requires",
    );
    let declared = CR4_REQUIRED | (have & CR4_OPTIONAL);

    // `EFER`'s three bits are checked here for the same reason `CR4`'s are:
    // setting a bit this CPU does not define is `#GP`, and on the BSP that is a
    // triple fault before `idt::init` with nothing to read. `SYSCALL` and `NX`
    // are both `CPUID.80000001H:EDX` (SDM Vol. 2A Table 3-8), and the extended
    // leaf itself has to be there before its bits mean anything: below
    // `0x8000_0001` a read of it answers with the highest basic leaf's
    // registers.
    let (max_ext, _, _, _) = cpu::cpuid(0x8000_0000, 0);
    let ext_edx = if max_ext >= 0x8000_0001 { cpu::cpuid(0x8000_0001, 0).3 } else { 0 };
    assert!(
        ext_edx & (1 << 11) != 0,
        "control_regs: cpu{cpu_id} has no SYSCALL/SYSRET, which is this kernel's only \
         way into and out of Ring 3",
    );
    assert!(
        ext_edx & (1 << 20) != 0,
        "control_regs: cpu{cpu_id} has no NX bit, so no mapping this kernel writes could \
         be made non-executable and W^X would silently not exist",
    );

    // Changing `LA57` with paging on is `#GP`, so a wholesale write cannot be
    // the thing that discovers firmware chose 5-level paging under a kernel
    // whose page tables are 4-level.
    let live = cpu::read_cr4();
    assert!(
        live & cr4::LA57 == 0,
        "control_regs: cpu{cpu_id} is in 5-level paging and this kernel's page tables are 4-level",
    );

    match DECLARED_CR4.compare_exchange(0, declared, Ordering::Release, Ordering::Acquire) {
        Ok(_) => declared,
        Err(published) => {
            assert!(
                published == declared,
                "control_regs: cpu{cpu_id} computes cr4={declared:#010x} and the machine \
                 declared {published:#010x} — its CPUs do not offer the same features",
            );
            declared
        }
    }
}

/// The `CR4` bits this CPU will accept, as CPUID reports them.
fn supported() -> u64 {
    const CPUID_1_EDX: [(u32, u64); 5] = [
        (2, cr4::DE),
        (6, cr4::PAE),
        (7, cr4::MCE),
        (24, cr4::OSFXSR),
        (25, cr4::OSXMMEXCPT),
    ];
    const CPUID_7_EBX: [(u32, u64); 3] =
        [(0, cr4::FSGSBASE), (7, cr4::SMEP), (20, cr4::SMAP)];
    const CPUID_7_ECX: [(u32, u64); 1] = [(2, cr4::UMIP)];

    let (max_leaf, _, _, _) = cpu::cpuid(0, 0);
    let (_, _, ecx1, edx1) = cpu::cpuid(1, 0);
    // A leaf above the maximum answers with the highest basic leaf's registers
    // rather than faulting, so an unguarded read here can report `FSGSBASE` off
    // somebody else's data — and a `CR4` bit the CPU does not define is the
    // triple fault the CPUID gating exists to replace with a named refusal.
    // Zero instead gives `declaration`'s assertion, which names the CPU.
    let (_, ebx7, ecx7, _) = if max_leaf >= 7 { cpu::cpuid(7, 0) } else { (0, 0, 0, 0) };

    let mut have = 0;
    for (bit, flag) in CPUID_1_EDX {
        if edx1 & (1 << bit) != 0 {
            have |= flag;
        }
    }
    for (bit, flag) in CPUID_7_EBX {
        if ebx7 & (1 << bit) != 0 {
            have |= flag;
        }
    }
    for (bit, flag) in CPUID_7_ECX {
        if ecx7 & (1 << bit) != 0 {
            have |= flag;
        }
    }
    // PCID without INVPCID is not worth having: the targeted flush is the whole
    // reason to carry process identifiers in the TLB.
    if ecx1 & (1 << 17) != 0 && ebx7 & (1 << 10) != 0 {
        have |= cr4::PCIDE;
    }
    have
}

/// One line per CPU naming what it holds, and then the assertion.
///
/// Per CPU rather than once for the machine, unlike the feature line beside it:
/// "every CPU answers this identically" is the assumption that was false, and a
/// summary is exactly the shape that hid it. Printed *before* the check, so a
/// CPU that fails leaves the value it failed with in the log rather than only a
/// verdict about it.
fn self_check(cpu_id: u32, declared_cr4: u64) {
    let live_cr0 = cpu::read_cr0();
    let live_cr4 = cpu::read_cr4();
    let live_efer = cpu::rdmsr(efer::MSR);
    log!(
        "control_regs: cpu{} cr0={:#010x} cr4={:#010x} efer={:#06x}{}{}{}{}{}",
        cpu_id,
        live_cr0,
        live_cr4,
        live_efer,
        opt(live_cr4, cr4::SMEP, " smep"),
        opt(live_cr4, cr4::SMAP, " smap"),
        opt(live_cr4, cr4::PCIDE, " pcid"),
        opt(live_cr4, cr4::UMIP, " umip"),
        opt(live_efer, efer::NXE, " nx"),
    );
    // **In the order this file declares them**, which is also the order the
    // line above prints and the order `init_cr0` then `init` applies them. A
    // CPU that diverges usually diverges in all three at once — `skipped` is
    // exactly that CPU — and which assertion fires first is then the whole of
    // what a reader sees, so it may not depend on the order somebody happened
    // to add a register in.
    assert!(
        live_cr0 == CR0,
        "control_regs: cpu{cpu_id} holds cr0={live_cr0:#010x}, the declaration is {CR0:#010x}",
    );
    assert!(
        live_cr4 == declared_cr4,
        "control_regs: cpu{cpu_id} holds cr4={live_cr4:#010x}, the declaration is \
         {declared_cr4:#010x}",
    );
    // `LMA` out of the comparison and not out of the log: it is the CPU's
    // answer about itself, and a CPU that has cleared it has left long mode.
    assert!(
        live_efer & !efer::LMA == EFER && live_efer & efer::LMA != 0,
        "control_regs: cpu{cpu_id} holds efer={live_efer:#06x}, the declaration is \
         {EFER:#06x} plus the CPU's own LMA",
    );
}

fn opt(value: u64, bit: u64, name: &'static str) -> &'static str {
    if value & bit != 0 { name } else { "" }
}

/// What an AP's caching was worth, measured on the CPU itself and on both sides
/// of the one instruction that turns it on.
///
/// **The dev host cannot answer this and no test asserts on it.** QEMU's TCG
/// models no cache, so `CR0.CD` there is a bit with no timing consequence, and
/// a KVM guest does not hold the bit at all — an AP that never cleared `CD`
/// reads it clear (`issues/kernel/ap-control-registers-inherit-init.md`).
/// The number is bare metal's, not a VM on
/// it, and the owner takes it with
/// `--diag-boot --kernel-param control-regs-bench`, off the panel.
///
/// Nothing outside the kernel can ask. There is no CPU affinity, so no userland
/// loop can choose the core it runs on, and the state under test lives only
/// between an AP's `INIT` and its first `mov cr0` — a window with no userland in
/// it at all. **The BSP's own row is the control**: it arrives with caching
/// already on, so its three numbers are the instrument's spread rather than an
/// effect.
#[cfg(feature = "boot-actuators")]
mod bench {
    use super::cpu;
    use crate::log;

    /// 4096 cache lines: bigger than any L1 and inside every L2 this kernel
    /// targets, so the warm pass measures a cache hit and the pre pass measures
    /// a bus transaction per line.
    const LINES: usize = 4096;
    const STRIDE: usize = 8;
    static PROBE: [u64; LINES * STRIDE] = [0; LINES * STRIDE];

    pub fn sample() -> u64 {
        if !crate::actuator::control_regs_bench() {
            return 0;
        }
        let start = cpu::rdtsc();
        let mut acc = 0u64;
        let mut i = 0;
        while i < PROBE.len() {
            // SAFETY: `i < PROBE.len()` is the loop condition, so the index is
            // in bounds and the pointer is into a live `static`. `read_volatile`
            // because the whole measurement is that the load actually happens —
            // a plain read of a zeroed `static` is one the optimiser may fold to
            // a constant, and then the number is the instrument's own noise.
            acc = acc.wrapping_add(unsafe { core::ptr::read_volatile(&raw const PROBE[i]) });
            i += STRIDE;
        }
        let end = cpu::rdtsc();
        core::hint::black_box(acc);
        end.wrapping_sub(start)
    }

    pub fn report(cpu_id: u32, before: u64) {
        if !crate::actuator::control_regs_bench() {
            return;
        }
        let cold = sample();
        let warm = sample();
        log!(
            "control_regs: cpu{} probe {} lines: pre={} cold={} warm={} cycles",
            cpu_id, LINES, before, cold, warm,
        );
    }
}

#[cfg(not(feature = "boot-actuators"))]
mod bench {
    pub fn sample() -> u64 {
        0
    }
    pub fn report(_cpu_id: u32, _before: u64) {}
}

/// The negative control. Leaves an AP holding what `INIT` left it, which is the
/// machine every boot before this file was; nothing else can stage it, because
/// a control register is the guest's own to write and no QEMU flag reaches one.
///
/// The check and its log line are the shipped ones, so what a run with this
/// armed produces is a real divergent CPU and a real failure.
fn skipped(cpu_id: u32) -> bool {
    crate::actuator::no_ap_control_regs() && cpu_id != 0
}

