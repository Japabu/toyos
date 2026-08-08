//! The one bracket every transition out of Ring 3 uses.
//!
//! `specs/user-machine-state.md` is the invariant: a transition out of Ring 3
//! that can reach another task must save and restore **the whole** user machine
//! state. These macros are the only text in the kernel naming an FP
//! instruction, so "saved some of it" is not expressible — there is nothing to
//! say it with.
//!
//! **Contract.** [`save_user_state`] may be invoked at any stack alignment; it
//! leaves `rsp` aligned to [`UserFpState`]'s alignment, which is also what a
//! System V call taken after it needs. [`restore_user_state`] puts `rsp` back
//! exactly where the save found it. Between them `r11` is scratch: every entry
//! has already pushed it as part of its GPR save, and one that has not would
//! lose the user's `r11` regardless.
//!
//! **The area is sized by the type, at every site, without the site saying so.**
//! [`ring3_naked_asm`] appends the two `const` operands the templates name, so
//! there is nowhere to write a number. That is the whole of the AVX-512 guard:
//! `XCR0` is 1 today and `FXSAVE64` is therefore complete, and enabling any
//! further state component means [`UserFpState`] grows and every reservation
//! grows with it or the build stops. A comment saying the same thing would not.

use core::mem::{align_of, size_of};

use super::fpu::UserFpState;

// The bracket reserves `fp_bytes + fp_align` and aligns down, so the area fits
// whatever the entry's incoming alignment was, and stashes the caller's `rsp`
// in the slack immediately above it.
const _: () = assert!(size_of::<UserFpState>() % align_of::<UserFpState>() == 0);
const _: () = assert!(align_of::<UserFpState>() >= 8);

/// `naked_asm!` for an entry that can reach another task, with the save area's
/// size and alignment supplied from [`UserFpState`].
///
/// The body must end with a trailing comma, as every `naked_asm!` in this
/// kernel already does.
macro_rules! ring3_naked_asm {
    ($($body:tt)*) => {
        core::arch::naked_asm!(
            $($body)*
            fp_bytes = const core::mem::size_of::<$crate::arch::fpu::UserFpState>(),
            fp_align = const core::mem::align_of::<$crate::arch::fpu::UserFpState>(),
        )
    };
}

/// Park the user machine state on this kernel stack.
macro_rules! save_user_state {
    () => {
        concat!(
            "mov r11, rsp\n",
            "sub rsp, {fp_bytes}\n",
            "sub rsp, {fp_align}\n",
            "and rsp, -{fp_align}\n",
            "mov [rsp + {fp_bytes}], r11\n",
            // Transitional body: XMM0-15 and MXCSR only, at their FXSAVE64
            // image offsets, so nothing but these lines moves when it becomes
            // one instruction.
            "stmxcsr [rsp + 24]\n",
            "movdqu [rsp + 160 + 0*16], xmm0\n",
            "movdqu [rsp + 160 + 1*16], xmm1\n",
            "movdqu [rsp + 160 + 2*16], xmm2\n",
            "movdqu [rsp + 160 + 3*16], xmm3\n",
            "movdqu [rsp + 160 + 4*16], xmm4\n",
            "movdqu [rsp + 160 + 5*16], xmm5\n",
            "movdqu [rsp + 160 + 6*16], xmm6\n",
            "movdqu [rsp + 160 + 7*16], xmm7\n",
            "movdqu [rsp + 160 + 8*16], xmm8\n",
            "movdqu [rsp + 160 + 9*16], xmm9\n",
            "movdqu [rsp + 160 + 10*16], xmm10\n",
            "movdqu [rsp + 160 + 11*16], xmm11\n",
            "movdqu [rsp + 160 + 12*16], xmm12\n",
            "movdqu [rsp + 160 + 13*16], xmm13\n",
            "movdqu [rsp + 160 + 14*16], xmm14\n",
            "movdqu [rsp + 160 + 15*16], xmm15\n",
        )
    };
}

/// Put it back, and `rsp` with it.
macro_rules! restore_user_state {
    () => {
        concat!(
            "movdqu xmm0,  [rsp + 160 + 0*16]\n",
            "movdqu xmm1,  [rsp + 160 + 1*16]\n",
            "movdqu xmm2,  [rsp + 160 + 2*16]\n",
            "movdqu xmm3,  [rsp + 160 + 3*16]\n",
            "movdqu xmm4,  [rsp + 160 + 4*16]\n",
            "movdqu xmm5,  [rsp + 160 + 5*16]\n",
            "movdqu xmm6,  [rsp + 160 + 6*16]\n",
            "movdqu xmm7,  [rsp + 160 + 7*16]\n",
            "movdqu xmm8,  [rsp + 160 + 8*16]\n",
            "movdqu xmm9,  [rsp + 160 + 9*16]\n",
            "movdqu xmm10, [rsp + 160 + 10*16]\n",
            "movdqu xmm11, [rsp + 160 + 11*16]\n",
            "movdqu xmm12, [rsp + 160 + 12*16]\n",
            "movdqu xmm13, [rsp + 160 + 13*16]\n",
            "movdqu xmm14, [rsp + 160 + 14*16]\n",
            "movdqu xmm15, [rsp + 160 + 15*16]\n",
            "ldmxcsr [rsp + 24]\n",
            "mov rsp, [rsp + {fp_bytes}]\n",
        )
    };
}

pub(crate) use {restore_user_state, ring3_naked_asm, save_user_state};
