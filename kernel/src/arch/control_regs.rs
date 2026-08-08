//! What `CR0` and `CR4` hold on every CPU in this machine.
//!
//! One declaration, applied by the BSP and by every AP, and checked on each of
//! them afterwards. Before this file there was no declaration at all: the BSP
//! ran with whatever firmware had left and an AP ran with the `INIT` value plus
//! the two bits `smp.rs`'s trampoline has to OR in to reach long mode, so cores
//! 1..N booted with **caching disabled**, `WP` clear and `NE` clear for the
//! whole history of the tree.
//!
//! Both registers are written whole, so every bit of both is decided here.
//! `CR0`'s value is a constant; three of `CR4`'s bits are the silicon's to
//! offer, so its value is *required plus whatever of the optional set this CPU
//! has* — a function every CPU evaluates and has to agree on.

use core::sync::atomic::{AtomicU64, Ordering};

use super::cpu;
use crate::log;

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
/// - `TS` (3) clear — lazy FP switching is ruled out
///   (`specs/user-machine-state.md` §6.3), so nothing ever sets it and `#NM`
///   keeps its meaning of "a userland bug".
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
/// programs no debug register, and `DE` clear is the 386 behaviour where `DR4`
/// and `DR5` alias `DR6` and `DR7` instead of raising `#UD` (SDM Vol. 3B
/// §17.2.2, *Debug Registers DR4 and DR5*). All five are older than x86-64 and
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
const CR4_OPTIONAL: u64 = cr4::SMEP | cr4::SMAP | cr4::PCIDE;

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
            unsafe {
                cpu::write_cr0((live | cr0::CD) & !cr0::NW);
                cpu::wbinvd();
            }
        }
        unsafe { cpu::write_cr0(CR0) };
    }
    bench::report(cpu_id, before);
}

/// Put this CPU's `CR4` into the declaration, then check that this CPU holds
/// both registers the declaration names.
///
/// Later than [`init_cr0`] because `SMEP` and `SMAP` are statements about the
/// address space: they are set once the CPU is on the kernel's own page tables,
/// not on the bootloader's.
pub fn init(cpu_id: u32) {
    let declared = declaration(cpu_id);
    if !skipped(cpu_id) {
        unsafe { cpu::write_cr4(declared) };
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

    let (max_leaf, _, _, _) = cpu::cpuid(0, 0);
    let (_, _, ecx1, edx1) = cpu::cpuid(1, 0);
    // A leaf above the maximum answers with the highest basic leaf's registers
    // rather than faulting, so an unguarded read here can report `FSGSBASE` off
    // somebody else's data — and a `CR4` bit the CPU does not define is the
    // triple fault the CPUID gating exists to replace with a named refusal.
    // Zero instead gives `declaration`'s assertion, which names the CPU.
    let (_, ebx7, _, _) = if max_leaf >= 7 { cpu::cpuid(7, 0) } else { (0, 0, 0, 0) };

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
    log!(
        "control_regs: cpu{} cr0={:#010x} cr4={:#010x}{}{}{}",
        cpu_id,
        live_cr0,
        live_cr4,
        opt(live_cr4, cr4::SMEP, " smep"),
        opt(live_cr4, cr4::SMAP, " smap"),
        opt(live_cr4, cr4::PCIDE, " pcid"),
    );
    assert!(
        live_cr0 == CR0,
        "control_regs: cpu{cpu_id} holds cr0={live_cr0:#010x}, the declaration is {CR0:#010x}",
    );
    assert!(
        live_cr4 == declared_cr4,
        "control_regs: cpu{cpu_id} holds cr4={live_cr4:#010x}, the declaration is \
         {declared_cr4:#010x}",
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
/// reads it clear (known-issues §8). The number is bare metal's, not a VM on
/// it, and the owner takes it with
/// `--diag-boot --kernel-feature control-regs-bench`, off the panel.
///
/// Nothing outside the kernel can ask. There is no CPU affinity, so no userland
/// loop can choose the core it runs on, and the state under test lives only
/// between an AP's `INIT` and its first `mov cr0` — a window with no userland in
/// it at all. **The BSP's own row is the control**: it arrives with caching
/// already on, so its three numbers are the instrument's spread rather than an
/// effect.
#[cfg(feature = "control-regs-bench")]
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
        let start = cpu::rdtsc();
        let mut acc = 0u64;
        let mut i = 0;
        while i < PROBE.len() {
            acc = acc.wrapping_add(unsafe { core::ptr::read_volatile(&raw const PROBE[i]) });
            i += STRIDE;
        }
        let end = cpu::rdtsc();
        core::hint::black_box(acc);
        end.wrapping_sub(start)
    }

    pub fn report(cpu_id: u32, before: u64) {
        let cold = sample();
        let warm = sample();
        log!(
            "control_regs: cpu{} probe {} lines: pre={} cold={} warm={} cycles",
            cpu_id, LINES, before, cold, warm,
        );
    }
}

#[cfg(not(feature = "control-regs-bench"))]
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
/// The check and its log line are the shipped ones, so what a run under this
/// feature produces is a real divergent CPU and a real failure.
fn skipped(cpu_id: u32) -> bool {
    cfg!(feature = "no-ap-control-regs") && cpu_id != 0
}

