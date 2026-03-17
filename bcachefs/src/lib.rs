#![cfg_attr(not(feature = "std"), no_std)]
#![allow(dead_code)]

extern crate alloc;

mod block_io;
mod crc32c;
mod superblock;
mod alloc_bitmap;
mod btree;
mod fs;

pub use block_io::{BlockIO, BlockBuf, BlockNum, SliceBlockIO};
#[cfg(feature = "std")]
pub use block_io::VecBlockIO;
pub use fs::{Formatted, Mounted, ReadOnly, ReadWrite, FsError};
pub use superblock::Superblock;
