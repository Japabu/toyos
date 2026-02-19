#![cfg_attr(not(feature = "std"), no_std)]
extern crate alloc;

mod disk;
mod fs;

pub use disk::{BlockDevice, Disk};
pub use fs::SimpleFs;

#[cfg(feature = "std")]
pub use disk::VecDisk;

#[cfg(not(feature = "std"))]
pub use disk::SliceDisk;
