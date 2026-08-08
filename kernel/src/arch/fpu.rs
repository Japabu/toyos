//! The user machine state: what a transition out of Ring 3 has to preserve.
//!
//! `specs/user-machine-state.md` is the invariant and the reasoning. This file
//! owns the x86-64 half of it — eventually `arch/x86_64/fpu.rs`, once the
//! architecture split this tree owes actually happens.

use super::cpu;
use crate::log;

/// CPUID with both index registers, `rbx` saved by hand because Rust reserves
/// it as a general operand.
fn cpuid(leaf: u32, subleaf: u32) -> (u32, u32, u32, u32) {
    let eax: u32;
    let ebx: u32;
    let ecx: u32;
    let edx: u32;
    unsafe {
        core::arch::asm!(
            "push rbx",
            "cpuid",
            "mov {ebx:e}, ebx",
            "pop rbx",
            ebx = out(reg) ebx,
            inout("eax") leaf => eax,
            inout("ecx") subleaf => ecx,
            out("edx") edx,
            options(nomem),
        );
    }
    (eax, ebx, ecx, edx)
}

/// XCR0, which says which state components `XSAVE` would move.
///
/// # Safety
/// `CR4.OSXSAVE` must be set; `xgetbv` is `#UD` otherwise.
unsafe fn xgetbv0() -> u64 {
    let lo: u32;
    let hi: u32;
    core::arch::asm!(
        "xgetbv",
        in("ecx") 0u32,
        out("eax") lo,
        out("edx") hi,
        options(nomem, nostack, preserves_flags),
    );
    ((hi as u64) << 32) | lo as u64
}

/// One line per CPU naming the control registers and enumeration leaves that
/// decide what state exists on this machine.
///
/// Per CPU rather than once for the machine, unlike the feature line beside it:
/// the BSP inherits firmware's `CR4` while an AP builds its own from the INIT
/// value, so "every CPU answers this identically" is the assumption under test
/// rather than a reason not to print. A thread that migrates between two CPUs
/// that disagree about `XCR0` faults on an instruction that worked a moment ago.
pub fn log_state() {
    let cr0 = cpu::read_cr0();
    let cr4 = cpu::read_cr4();
    let (max_leaf, _, _, _) = cpuid(0, 0);
    let (_, _, ecx1, _) = cpuid(1, 0);
    let xsave = ecx1 & (1 << 26) != 0;
    let osxsave = ecx1 & (1 << 27) != 0;
    let xcr0 = if osxsave { unsafe { xgetbv0() } } else { 0 };
    let (d0a, d0b, d0c, _) = if max_leaf >= 0xD { cpuid(0xD, 0) } else { (0, 0, 0, 0) };
    let (d1a, _, _, _) = if max_leaf >= 0xD { cpuid(0xD, 1) } else { (0, 0, 0, 0) };
    log!(
        "fpu: cpu{} cr0={:#x} cr4={:#x} xsave={} osxsave={} xcr0={:#x} \
         cpuid.d.0=({:#x},{},{}) cpuid.d.1.eax={:#x}",
        super::percpu::cpu_id(),
        cr0,
        cr4,
        xsave as u8,
        osxsave as u8,
        xcr0,
        d0a,
        d0b,
        d0c,
        d1a,
    );
}
