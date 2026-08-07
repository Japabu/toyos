//! Intel HDA codec decoding and output-path selection, as pure functions.
//!
//! Everything a driver has to *decide* before it touches a register: what a
//! codec said about itself, and which converter and pins carry sound to a
//! speaker. No I/O, no register writes, no allocation of device memory — the
//! effects are soundd's. `specs/hda-driver-plan.md` §5.2 is why this is a
//! crate: the traversal is the least-covered code in that plan, and here it is
//! testable against the graph of the machine that has to work.
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
