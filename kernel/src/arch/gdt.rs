// GDT is now per-CPU, owned by PerCpu in percpu.rs.
// This module re-exports constants for existing callers.

pub use super::percpu::KERNEL_CS;
pub use super::percpu::KERNEL_DS;
