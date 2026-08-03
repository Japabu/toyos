//! The `/home` filesystem, named for its destination rather than its present
//! contents: ToyOS will implement bcachefs and be compatible with its on-disk
//! format, and this crate is where that lands. What it holds today is an
//! interim ToyOS-designed format sharing nothing with bcachefs but the
//! ambition.

#![cfg_attr(not(feature = "std"), no_std)]
#![allow(dead_code)]

extern crate alloc;

mod block_io;
mod crc32c;
mod superblock;
mod alloc_bitmap;
mod btree;
mod fs;

pub use block_io::{BlockIO, BlockBuf, BlockNum, DeviceError, SliceBlockIO};
#[cfg(feature = "std")]
pub use block_io::VecBlockIO;
pub use fs::{Formatted, Mounted, ReadOnly, ReadWrite, FsError, Extent};
pub use superblock::{DESIGNATION_BLOCKS_OFFSET, DESIGNATION_MAGIC, Superblock};

/// Records the largest single allocation each test thread makes, so a test can
/// assert what parsing a crafted block asks the allocator for.
///
/// Nothing else can see that. The defect this exists to catch — a `Vec` sized
/// from an on-disk `u16` — leaves the parse returning the same error either
/// way, so a test that only checks the return value passes with the bug in.
#[cfg(test)]
mod alloc_probe {
    use core::alloc::{GlobalAlloc, Layout};
    use core::cell::Cell;
    use std::alloc::System;

    std::thread_local! {
        static PEAK: Cell<usize> = const { Cell::new(0) };
    }

    pub struct Probe;

    fn record(size: usize) {
        // `try_with` because TLS destruction can free after the slot is gone.
        let _ = PEAK.try_with(|p| p.set(p.get().max(size)));
    }

    /// The largest single allocation this thread has made since the last call.
    pub fn take_peak() -> usize {
        PEAK.with(|p| p.replace(0))
    }

    unsafe impl GlobalAlloc for Probe {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            record(layout.size());
            unsafe { System.alloc(layout) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) }
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            record(new_size);
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }

    #[global_allocator]
    static PROBE: Probe = Probe;
}
