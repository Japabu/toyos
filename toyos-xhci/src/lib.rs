//! The xHCI root-hub port machine, as a decision separated from its effects.
//!
//! The kernel reads PORTSC, asks [`port::PortState::step`] what to do, and does
//! it; nothing here touches a register, a ring or a slot. That split is what
//! lets a host simulator explore the port state space, which is where the
//! T14's SuperSpeed wedge lives — a state QEMU cannot produce.

#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]

pub mod enumerate;
pub mod invariants;
pub mod job;
pub mod port;
pub mod protocol;
pub mod portsc;
pub mod recovery;

pub use job::{Await, Outcome, Outstanding};
pub use port::{Effect, Gone, Nanos, PortState, Step};
pub use portsc::{LinkState, Portsc};
pub use protocol::{Protocol, Protocols};
pub use recovery::{EndpointState, Recovery};
