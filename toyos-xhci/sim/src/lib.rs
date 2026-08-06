//! The host simulator for the xHCI port machine.
//!
//! A fake port register with hardware's own write rules, the loop the kernel
//! runs, and the scenarios and negative gates that decide whether the machine
//! is right. `specs/xhci-port-machine-plan.md` §3 X0.

pub mod driver;
pub mod hub;
