//! The one bracket every transition out of Ring 3 uses.
//!
//! **The invariant**: a transition out of Ring 3 that can reach another task
//! must save and restore **the whole** user machine state, and restore it as
//! the transition's last act before returning to Ring 3 — after any point at
//! which the task could have been switched. These macros are the only text in
//! the kernel naming an FP instruction, so "saved some of it" is not
//! expressible — there is nothing to say it with.
//!
//! **Contract.** [`save_user_state`] may be invoked at any stack alignment; it
//! leaves `rsp` aligned to [`UserFpState`]'s alignment, which is also what a
//! System V call taken after it needs. [`restore_user_state`] puts `rsp` back
//! exactly where the save found it. Between them `r11` is scratch: every entry
//! has already pushed it as part of its GPR save, and one that has not would
//! lose the user's `r11` regardless.
//!
//! **What it saves is everything this kernel permits to exist.** `CR4.OSXSAVE`
//! is set nowhere, so `XCR0` is 1 and `FXSAVE64` is complete rather than cheap.
//! Every user thread's x87 register file, control, status and tag words, XMM0-15
//! and `MXCSR` cross a ring transition intact — including a *pending unmasked
//! x87 exception*, which `FXSAVE64` carries across without raising it, so it
//! reaches only the task that caused it. It used to be left on the CPU for
//! whatever ran next.
//!
//! **The area is sized by the type, at every site, without the site saying so.**
//! [`ring3_naked_asm`] appends the two `const` operands the templates name, so
//! there is nowhere to write a number. That is the whole of the AVX-512 guard:
//! `XCR0` is 1 today and `FXSAVE64` is therefore complete, and enabling any
//! further state component means [`UserFpState`] grows and every reservation
//! grows with it or the build stops. A comment saying the same thing would not.

use core::mem::{align_of, size_of};

use super::fpu::UserFpState;

/// A gate whose handler can reach another task before it returns to Ring 3, and
/// which therefore brackets the user machine state.
///
/// The type does not prove the bracket is there — nothing short of reading the
/// assembly does. What it makes unrepresentable is installing a handler
/// *without answering the question*, which is the same move `idt_vectors!`
/// makes one level up for the error-code form: the classification is a column
/// in one table and cannot silently disagree with the slot it fills.
#[derive(Clone, Copy)]
pub struct Ring3Entry(unsafe extern "sysv64" fn());

/// A gate whose handler cannot reach another task, so it saves nothing.
///
/// There are two, and each says why at its row in [`idt_vectors`].
///
/// [`idt_vectors`]: super::idt
#[derive(Clone, Copy)]
pub struct Ring0Entry(unsafe extern "sysv64" fn());

impl Ring3Entry {
    pub const fn new(handler: unsafe extern "sysv64" fn()) -> Self {
        Self(handler)
    }

    pub fn addr(self) -> u64 {
        self.0 as *const () as u64
    }
}

impl Ring0Entry {
    pub const fn declare(handler: unsafe extern "sysv64" fn()) -> Self {
        Self(handler)
    }

    pub fn addr(self) -> u64 {
        self.0 as *const () as u64
    }
}

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

/// `naked_asm!` for a trampoline that puts a thread into Ring 3 for the first
/// time, with the declared state's address supplied from [`fpu`].
///
/// [`fpu`]: super::fpu
#[cfg(not(feature = "fpu-save-nothing"))]
macro_rules! ring3_trampoline_asm {
    ($($body:tt)*) => {
        core::arch::naked_asm!(
            $($body)*
            fp_initial = sym $crate::arch::fpu::INITIAL_IMAGE,
        )
    };
}

/// The state a task that has never been in Ring 3 starts from.
#[cfg(not(feature = "fpu-save-nothing"))]
macro_rules! initial_user_state {
    () => {
        "fxrstor64 [rip + {fp_initial}]\n"
    };
}

/// Park the user machine state on this kernel stack.
#[cfg(not(feature = "fpu-save-nothing"))]
macro_rules! save_user_state {
    () => {
        concat!(
            "mov r11, rsp\n",
            "sub rsp, {fp_bytes}\n",
            "sub rsp, {fp_align}\n",
            "and rsp, -{fp_align}\n",
            "mov [rsp + {fp_bytes}], r11\n",
            // Non-waiting, which is the whole reason this family and not
            // FSAVE: a pending unmasked x87 exception must be *saved*, not
            // trapped on, and certainly not in Ring 0. The REX.W form, because
            // plain FXSAVE keeps only the low 32 bits of FIP and FDP.
            "fxsave64 [rsp]\n",
        )
    };
}

/// Put it back, and `rsp` with it.
#[cfg(not(feature = "fpu-save-nothing"))]
macro_rules! restore_user_state {
    () => {
        concat!(
            "fxrstor64 [rsp]\n",
            "mov rsp, [rsp + {fp_bytes}]\n",
        )
    };
}

// The negative control (`fpu-save-nothing`, declared in `kernel/Cargo.toml`).
// Everything the bracket does *except* move the state: the same reservation,
// the same alignment, the same rsp bookkeeping, so what the gate then observes
// is one missing instruction and not a different kernel.

#[cfg(feature = "fpu-save-nothing")]
macro_rules! ring3_trampoline_asm {
    ($($body:tt)*) => { core::arch::naked_asm!($($body)*) };
}

#[cfg(feature = "fpu-save-nothing")]
macro_rules! initial_user_state {
    () => {
        ""
    };
}

#[cfg(feature = "fpu-save-nothing")]
macro_rules! save_user_state {
    () => {
        concat!(
            "mov r11, rsp\n",
            "sub rsp, {fp_bytes}\n",
            "sub rsp, {fp_align}\n",
            "and rsp, -{fp_align}\n",
            "mov [rsp + {fp_bytes}], r11\n",
        )
    };
}

#[cfg(feature = "fpu-save-nothing")]
macro_rules! restore_user_state {
    () => {
        "mov rsp, [rsp + {fp_bytes}]\n"
    };
}

pub(crate) use {
    initial_user_state, restore_user_state, ring3_naked_asm, ring3_trampoline_asm, save_user_state,
};
