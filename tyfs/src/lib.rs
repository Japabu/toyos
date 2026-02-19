#![no_std]
extern crate alloc;

mod disk;
mod fs;

pub use disk::Disk;
pub use fs::SimpleFs;
