//! The xHCI root-hub port machine, as a decision separated from its effects.
//!
//! The kernel reads PORTSC, asks [`port::PortState::step`] what to do, and does
//! it; nothing here touches a register, a ring or a slot. That split is what
//! lets a host simulator explore the port state space, which is where the
//! T14's SuperSpeed wedge lives — a state QEMU cannot produce.
//!
//! `specs/xhci-port-machine-plan.md` is the design and the staging.

#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]

pub mod invariants;
pub mod port;
pub mod portsc;

pub use port::{Effect, Gone, Nanos, PortState, Step};
pub use portsc::Portsc;
