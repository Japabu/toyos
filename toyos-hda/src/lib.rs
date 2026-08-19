//! Intel HDA codec decoding and output-path selection, as pure functions.
//!
//! Everything a driver has to *decide* before it touches a register: what a
//! codec said about itself, and which converter and pins carry sound to a
//! speaker. No I/O, no register writes, no allocation of device memory — the
//! effects are soundd's. It is a crate because the graph traversal is what QEMU
//! certifies least: pure, it is host-tested against the committed codec dumps of
//! the machines that have to work, and against states no machine in reach
//! constructs.
//!
//! Every value a codec supplies is untrusted. Counts are bounded, node ids are
//! checked against the range their function group declared, and a connection
//! list that cycles is a refusal rather than a walk.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod caps;
pub mod config;
pub mod graph;
pub mod path;
pub mod probe;
pub mod stream;
pub mod verb;

#[cfg(test)]
mod fixture;

pub use caps::{AmpCaps, ConfigDefault, GainRange, PinCaps, WidgetCaps, WidgetKind};
pub use graph::{Codec, FunctionGroup, Widget};
pub use path::{find_output_path, Hop, OutputPath, PathError, PinSetup};
pub use probe::{enumerate, CodecFault, Verbs};
pub use verb::{Address, Node, Response, Verb};
