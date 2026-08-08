//! The user machine state: what a transition out of Ring 3 has to preserve.
//!
//! `specs/user-machine-state.md` is the invariant and the reasoning. This file
//! owns the x86-64 half of it — eventually `arch/x86_64/fpu.rs`, once the
//! architecture split this tree owes actually happens.

use super::cpu;
use crate::log;

/// The whole of the user machine state this kernel permits to exist, in the
/// layout `FXSAVE64` writes and `FXRSTOR64` reads.
///
/// `XCR0` is 1 on every machine this kernel boots — `CR4.OSXSAVE` is set
/// nowhere, so nothing can widen it — which is what makes 512 bytes *complete*
/// rather than a cheap approximation. The day any further component is enabled,
/// `FXSAVE64` becomes a silent partial save and this type has to grow with the
/// move to `XSAVE`; every reservation in `arch::entry` is sized from
/// `size_of::<Self>()` so that growth cannot be forgotten in one of them.
#[repr(C, align(16))]
#[derive(Clone, Copy)]
pub struct UserFpState([u8; 512]);

/// The x87 control word every exception masked, extended precision, round to
/// nearest — the value `FNINIT` leaves.
const FCW_INITIAL: u16 = 0x037F;

/// `MXCSR` every exception masked, round to nearest, no flush-to-zero — the
/// value the SDM gives as the power-on default.
const MXCSR_INITIAL: u32 = 0x1F80;

// Field offsets in the FXSAVE64 image (SDM Vol. 1 Table 10-2).
const OFF_FCW: usize = 0;
const OFF_MXCSR: usize = 24;
/// The CPU writes its own mask here, so it is a property of the silicon rather
/// than of the state, and the self-check cannot compare it against anything.
const OFF_MXCSR_MASK: usize = 28;
const OFF_ST0: usize = 32;
/// Past the last XMM register. Bytes above this are reserved or available for
/// software, and `FXSAVE` is not obliged to write them.
const END_XMM: usize = 416;

impl UserFpState {
    /// The declared state a task that has never been in Ring 3 starts from.
    pub const INITIAL: Self = {
        let mut b = [0u8; 512];
        let fcw = FCW_INITIAL.to_le_bytes();
        b[OFF_FCW] = fcw[0];
        b[OFF_FCW + 1] = fcw[1];
        let mxcsr = MXCSR_INITIAL.to_le_bytes();
        b[OFF_MXCSR] = mxcsr[0];
        b[OFF_MXCSR + 1] = mxcsr[1];
        b[OFF_MXCSR + 2] = mxcsr[2];
        b[OFF_MXCSR + 3] = mxcsr[3];
        Self(b)
    };

    /// This CPU's state, now.
    ///
    /// The Rust form of what `arch::entry`'s `save_user_state!` does in naked
    /// assembly; both write this type's layout and neither can drift from it,
    /// because the macro sizes its reservation from `size_of::<Self>()`.
    pub fn saved_from_cpu() -> Self {
        let mut state = Self([0u8; 512]);
        unsafe {
            core::arch::asm!(
                "fxsave64 [{}]",
                in(reg) &raw mut state.0,
                options(nostack),
            );
        }
        state
    }

    /// The bytes `FXSAVE64` defines and two states may be compared on: the
    /// header up to `MXCSR_MASK`, then the x87 and XMM register files.
    ///
    /// `MXCSR_MASK` is the CPU's answer and not part of the state. Everything
    /// above the XMM area is reserved or software-available, and `FXSAVE` may
    /// leave it as it found it.
    fn defined(&self) -> (&[u8], &[u8]) {
        (&self.0[..OFF_MXCSR_MASK], &self.0[OFF_ST0..END_XMM])
    }

    fn matches(&self, other: &Self) -> bool {
        self.defined() == other.defined()
    }

    /// Load this state onto the CPU.
    ///
    /// # Safety
    /// `MXCSR` in the image must have no reserved bit set, or `FXRSTOR64` is
    /// `#GP`. Both constructors satisfy that: [`Self::INITIAL`] is a literal
    /// and [`Self::saved_from_cpu`] is what the CPU itself wrote.
    pub unsafe fn restore(&self) {
        core::arch::asm!(
            "fxrstor64 [{}]",
            in(reg) &raw const self.0,
            options(nostack, readonly),
        );
    }
}

/// [`UserFpState::INITIAL`] in memory, so the loader's trampolines can put a
/// new thread into it with one instruction and no stack of their own.
///
/// A trampoline cannot use `FNINIT` instead: it marks the x87 registers empty
/// without clearing them, so an `FXSAVE` reads the previous tenant's data back
/// out, and it does not touch XMM at all.
pub static INITIAL_IMAGE: UserFpState = UserFpState::INITIAL;

/// Put this CPU's FPU into the declared state and check that it is the state we
/// think it is.
///
/// Called once per CPU, after `CR4.OSFXSR` is set — `LDMXCSR` is `#UD`
/// otherwise.
pub fn init() {
    load_initial();
    self_check();
}

/// `FNINIT` for the x87 half, `LDMXCSR` for the SSE half. Neither waits, so
/// this is safe to run over an FPU left holding a pending unmasked exception.
fn load_initial() {
    unsafe {
        core::arch::asm!(
            "fninit",
            "ldmxcsr [{}]",
            in(reg) &MXCSR_INITIAL,
            options(nostack, readonly),
        );
    }
}

/// Assert that what the CPU calls the architectural default is what
/// [`UserFpState::INITIAL`] says it is.
///
/// A kernel bug if it is not: the value would be restored onto every new thread
/// and would be wrong on every one of them, silently. The caller has just run
/// [`load_initial`], so this compares like with like.
fn self_check() {
    assert_eq!(
        (&raw const INITIAL_IMAGE) as usize % core::mem::align_of::<UserFpState>(),
        0,
        "fpu: the initial image is misaligned, so every trampoline would #GP",
    );
    let live = UserFpState::saved_from_cpu();
    assert!(
        live.matches(&UserFpState::INITIAL),
        "fpu: cpu{} disagrees about the architectural default state — \
         fcw={:#06x} mxcsr={:#010x}, expected fcw={:#06x} mxcsr={:#010x}",
        super::percpu::cpu_id(),
        u16::from_le_bytes([live.0[OFF_FCW], live.0[OFF_FCW + 1]]),
        u32::from_le_bytes([
            live.0[OFF_MXCSR],
            live.0[OFF_MXCSR + 1],
            live.0[OFF_MXCSR + 2],
            live.0[OFF_MXCSR + 3],
        ]),
        FCW_INITIAL,
        MXCSR_INITIAL,
    );
}

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
