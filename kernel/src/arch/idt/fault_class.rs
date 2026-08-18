//! Whose fault a trap was — the one classification the crash path makes, and
//! the only thing deciding whether a process dies or the machine halts.
//!
//! **The ring is a fact the frame carries, and this file exists so that nothing
//! guesses it from anything else.** The decision used to be spelled
//! `is_user_mode() || (PageFault && current_tid().is_some() && cr2 < USER_TOP)`,
//! and the second disjunct answers *user* for any low faulting address however
//! Ring 0 the frame was. On 2026-08-17 the kernel fetched an instruction from
//! address zero under `sched_stress` —
//! `#PF UNHANDLED: cr2=0x0 rip=0x0 err=0x10 user=false tid=Some(Tid(0))` — the
//! disjunct held on `cr2 = 0` and a live tid, and the kernel went down the
//! *recover* path for a null jump instead of halting on it. The `KERNEL PANIC`
//! and the crash report that would have named the null call were therefore
//! never written, the shared boot said nothing for the whole 88 s guard, and
//! the defect underneath is still unbisectable
//! (`issues/kernel/the-shared-boot-jumped-to-null-spawning-sched-stress.md`).
//!
//! [`Ring`] is the repair. It is opaque and its one constructor takes a code
//! segment selector, so a privilege level cannot be read out of a faulting
//! address by any caller: the confusion is unwritable rather than fixed at one
//! call site. What no type can settle is the genuinely ambiguous case that
//! second disjunct existed for — Ring 0 code dereferencing a pointer that
//! crossed the syscall boundary, which must kill the process and not the
//! machine — so [`blame`] answers that one at runtime, from the two addresses
//! the frame carries rather than from one of them.
//!
//! This file names exactly one thing outside itself, the user/kernel bound in
//! `kernel/src/mm/user_span.rs`. That is what lets `kernel-fault/` compile both
//! and run the table below on the host, against the same constant the kernel
//! reads rather than a copy of it.

use crate::mm::user_span::is_user_addr;

/// The privilege level a trap frame arrived from.
///
/// Opaque, and there is one constructor: the frame's own `cs`. A `Ring` is
/// therefore never anything but what the hardware pushed, which is the whole
/// point of the type — the classification below cannot be handed a ring
/// somebody inferred.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ring(bool);

impl Ring {
    /// The ring a code segment selector names. The RPL field is the low two
    /// bits, and this kernel runs Ring 0 and Ring 3 only.
    pub const fn of_cs(cs: u64) -> Self {
        Self(cs & 3 != 0)
    }

    /// Whether the frame was Ring 3.
    pub const fn is_user(self) -> bool {
        self.0
    }
}

/// The address a trap names, on the one vector that names one.
///
/// A #PF reports the linear address it could not translate in CR2; every other
/// vector leaves whatever was there last, so reading it would attribute a fault
/// to an address that has nothing to do with it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Faulted {
    /// A page fault, and what CR2 held.
    Address(u64),
    /// Any other vector: there is no faulted address.
    Nothing,
}

/// Whose fault it was, and therefore what the kernel does next.
///
/// Three states, where the crash path used to carry two independent `bool`s
/// (`is_user`, `is_ring3`) — of whose four combinations one, "a user fault from
/// a frame that was not Ring 3 and not in a syscall either", meant nothing and
/// was still writable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Blame {
    /// A Ring 3 frame. The process did it, it holds no kernel lock, and the
    /// ordinary exit path can end it.
    Process,
    /// Ring 0 code, running on a thread's behalf, faulting on a *user* address:
    /// a pointer that crossed the syscall boundary. Still the process's, but
    /// the faulted thread may hold any kernel lock, so it goes out through the
    /// poison set rather than through the process table.
    ProcessThroughKernel,
    /// Ring 0, and nothing about it belongs to a process. The machine halts,
    /// after saying so.
    Kernel,
}

/// Who a fault belongs to.
///
/// `rip` is the faulting frame's instruction pointer and `on_a_thread` is
/// whether a thread is current on this CPU.
///
/// **A Ring 0 frame whose `rip` is not a kernel address is the kernel's however
/// low the faulted address is.** That is the sighting above: broken control
/// flow, not a bad pointer — the kernel is not *at* an instruction that could
/// be dereferencing anything on a thread's behalf. An instruction-fetch fault
/// needs no separate arm for the same reason: the address it could not fetch is
/// where `rip` now points, so it fails this test by itself.
pub fn blame(ring: Ring, rip: u64, faulted: Faulted, on_a_thread: bool) -> Blame {
    if ring.is_user() {
        return Blame::Process;
    }
    match faulted {
        Faulted::Address(addr)
            if on_a_thread && !is_user_addr(rip) && is_user_addr(addr) =>
        {
            Blame::ProcessThroughKernel
        }
        _ => Blame::Kernel,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A kernel `rip`, the shape every Ring 0 frame in this kernel has: the
    /// direct map starts at `0xFFFF_8000_0000_0000` and the crash reports quote
    /// addresses like `0xffff80007b519a02`.
    const KERNEL_RIP: u64 = 0xFFFF_8000_7B51_9A02;
    /// A user `rip`, from a PIE loaded at the base this kernel uses.
    const USER_RIP: u64 = 0x0000_0100_0009_7176;
    /// A Ring 3 code selector, and a Ring 0 one.
    const USER_CS: u64 = 0x2B;
    const KERNEL_CS: u64 = 0x08;

    #[test]
    fn a_ring_is_only_ever_what_cs_said() {
        assert!(Ring::of_cs(USER_CS).is_user());
        assert!(!Ring::of_cs(KERNEL_CS).is_user());
        // Every selector with a non-zero RPL is Ring 3 and no other, so a
        // faulting address cannot enter this answer at all.
        for rpl in 1..4u64 {
            assert!(Ring::of_cs(rpl).is_user(), "RPL {rpl} is not Ring 0");
        }
        assert!(!Ring::of_cs(0).is_user());
    }

    /// The sighting: Ring 0 fetching an instruction from address zero.
    ///
    /// It must be the kernel's whatever `current_tid()` says, because that is
    /// the only answer that prints `KERNEL PANIC` and halts. Under the old
    /// disjunct it was the process's, and the report never arrived.
    #[test]
    fn a_ring_0_fault_at_a_low_address_is_the_kernels() {
        assert_eq!(
            blame(Ring::of_cs(KERNEL_CS), 0, Faulted::Address(0), true),
            Blame::Kernel,
        );
        // Not a property of zero: any address in the user half, reached from a
        // Ring 0 frame that is not executing kernel code, is the same finding.
        assert_eq!(
            blame(Ring::of_cs(KERNEL_CS), USER_RIP, Faulted::Address(0x1B), true),
            Blame::Kernel,
        );
        // And with no thread current at all, which is the case the old spelling
        // did get right.
        assert_eq!(
            blame(Ring::of_cs(KERNEL_CS), 0, Faulted::Address(0), false),
            Blame::Kernel,
        );
    }

    /// The direction a careless fix breaks: a process that dereferences null
    /// must still be the process's, and must not take the machine with it.
    #[test]
    fn a_user_fault_at_a_low_address_is_still_the_processs() {
        assert_eq!(
            blame(Ring::of_cs(USER_CS), USER_RIP, Faulted::Address(0), true),
            Blame::Process,
        );
        // A Ring 3 frame is the process's on every vector, with or without a
        // faulted address: #UD, #GP and the arithmetic traps all arrive here.
        assert_eq!(
            blame(Ring::of_cs(USER_CS), USER_RIP, Faulted::Nothing, true),
            Blame::Process,
        );
    }

    /// The other half of that direction, and the one the old disjunct was
    /// written for: `SYS_DEBUG`'s `NULL_READ` reads address zero from *kernel*
    /// code inside a syscall, and `panic_recovery` asserts the caller dies and
    /// the machine lives.
    #[test]
    fn a_kernel_dereference_of_a_user_pointer_is_the_processs() {
        assert_eq!(
            blame(Ring::of_cs(KERNEL_CS), KERNEL_RIP, Faulted::Address(0), true),
            Blame::ProcessThroughKernel,
        );
        assert_eq!(
            blame(
                Ring::of_cs(KERNEL_CS),
                KERNEL_RIP,
                Faulted::Address(crate::mm::user_span::USER_TOP - 1),
                true,
            ),
            Blame::ProcessThroughKernel,
        );
        // No thread to attribute it to, so there is nothing to kill but the
        // machine's illusion that it is well.
        assert_eq!(
            blame(Ring::of_cs(KERNEL_CS), KERNEL_RIP, Faulted::Address(0), false),
            Blame::Kernel,
        );
    }

    /// A kernel address faulted from kernel code is the kernel's, which is what
    /// `idle_stack_guard` asserts by halting: the guard page under a per-CPU
    /// idle stack is read on purpose and the machine must stop.
    #[test]
    fn a_kernel_address_faulted_from_kernel_code_is_the_kernels() {
        assert_eq!(
            blame(
                Ring::of_cs(KERNEL_CS),
                KERNEL_RIP,
                Faulted::Address(0xFFFF_8000_0102_0FFF),
                true,
            ),
            Blame::Kernel,
        );
    }

    /// Every vector that is not a page fault carries no faulted address, so a
    /// Ring 0 one is the kernel's — a `#GP` inside a syscall halts rather than
    /// killing whichever process happened to be current.
    #[test]
    fn a_ring_0_trap_with_no_faulted_address_is_the_kernels() {
        assert_eq!(
            blame(Ring::of_cs(KERNEL_CS), KERNEL_RIP, Faulted::Nothing, true),
            Blame::Kernel,
        );
    }

    /// The bound itself, at both of the places the classification turns on it.
    #[test]
    fn the_user_kernel_bound_is_where_both_answers_change() {
        use crate::mm::user_span::USER_TOP;
        let k = Ring::of_cs(KERNEL_CS);
        // The faulted address, one byte either side.
        assert_eq!(
            blame(k, KERNEL_RIP, Faulted::Address(USER_TOP - 1), true),
            Blame::ProcessThroughKernel,
        );
        assert_eq!(blame(k, KERNEL_RIP, Faulted::Address(USER_TOP), true), Blame::Kernel);
        // And `rip`, on the same bound and in the other direction: the test is
        // "not in the user half", so the highest address userland can name is
        // the last `rip` that answers `Kernel`, and the first one past it — the
        // start of the non-canonical hole, which no frame can hold — is the
        // first that does not.
        assert_eq!(blame(k, USER_TOP - 1, Faulted::Address(0), true), Blame::Kernel);
        assert_eq!(
            blame(k, USER_TOP, Faulted::Address(0), true),
            Blame::ProcessThroughKernel,
        );
    }
}
