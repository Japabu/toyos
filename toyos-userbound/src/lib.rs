//! Every decision the kernel makes about the user/kernel boundary.
//!
//! Two questions and one line between them. **Before a dereference**: is this
//! address userland's, is the object at it aligned for the type being read, and
//! does it lie wholly inside one mapping? **After a trap**: which side did the
//! frame come from, and whose fault was it?
//!
//! [`span`] answers the first, [`fault`] the second — and the second is written
//! in terms of the first. A fault is classified against the same [`USER_TOP`]
//! the accessors refuse an address above, not against a copy of it, and that is
//! why the two are one crate rather than two: a second constant is a second
//! thing to get wrong, and [`blame`]'s whole argument is that the bound it
//! reads is the kernel's own.
//!
//! Pure. No I/O, no allocation, no `unsafe`, nothing read from a device and
//! nothing named outside this crate. The kernel is the only caller —
//! `user_ptr.rs`, `mm/`, `arch/syscall.rs`, `loader/` and
//! `arch/idt/exceptions.rs` — and this is a crate rather than files inside it so
//! that the boundary table below runs on the host in milliseconds instead of in
//! a boot.
//!
//! The numbers are x86-64's: [`USER_TOP`] is the canonical split at 48-bit
//! linear addresses, [`PAGE_2M`] is the kernel's one user page size, and a
//! [`Ring`] comes out of a code segment selector's RPL field. A second
//! architecture brings its own three; nothing else here changes.
//!
//! `specs/plans/memory-boundary-spec.md` is the argument behind [`span`].

#![no_std]
#![forbid(unsafe_code)]

pub mod fault;
pub mod span;

pub use fault::{blame, Blame, Faulted, Ring};
pub use span::{in_user_half, is_user_addr, is_user_object, PAGE_2M, USER_TOP};
