use alloc::vec::Vec;

use bcachefs::Extent;
use crate::block::{BlockError, BlockResult};
use crate::page_cache;

const BLOCK_SIZE: usize = 4096;
const BLOCK_SIZE_U64: u64 = 4096;

/// Abstracts the backing store for a memory-mapped file.
/// The page fault handler calls `read_page()` — it never knows
/// whether the data comes from NVMe, RAM, or anywhere else.
pub trait FileBacking: Send + Sync {
    /// Read one 4KB page of file data at `file_offset` into `buf`.
    /// If the offset extends beyond the file, zero-fill the remainder.
    ///
    /// `Err` means the store could not be read and `buf` holds zeros rather
    /// than the file's bytes — fallible for the same reason every
    /// [`BlockDevice`] method is: a hole and data must not be the same value.
    /// The caller that must not ignore it is [`file_cache::write_page`], which
    /// re-fetches through here before merging a partial write. Merging into a
    /// fetch that failed and then flushing the result is how a 4 KiB region of
    /// a file on disk becomes zeros.
    ///
    /// [`BlockDevice`]: crate::block::BlockDevice
    /// [`file_cache::write_page`]: crate::file_cache::write_page
    #[must_use = "a failed read left the buffer zeroed; it does not hold the file's bytes"]
    fn read_page(&self, file_offset: u64, buf: &mut [u8; BLOCK_SIZE]) -> BlockResult;

    /// Total file size in bytes.
    fn file_size(&self) -> u64;

    /// Return a pointer to file data at `offset` if it's contiguous in memory.
    /// Only works for memory-resident backings (e.g. initrd).
    fn memory_ptr(&self, _offset: u64, _len: usize) -> Option<*const u8> {
        None
    }
}

/// File backed by NVMe blocks via the kernel PageCache.
pub struct NvmeBacking {
    extents: Vec<Extent>,
    size: u64,
}

impl NvmeBacking {
    pub fn new(extents: Vec<Extent>, size: u64) -> Self {
        Self { extents, size }
    }

    /// Convert a file byte offset to an NVMe block number by walking extents.
    fn file_offset_to_block(&self, file_offset: u64) -> Option<u64> {
        let block_idx = file_offset / BLOCK_SIZE_U64;
        let mut cursor = 0u64;
        for ext in &self.extents {
            let count = ext.block_count as u64;
            if block_idx < cursor + count {
                return Some(ext.start_block + (block_idx - cursor));
            }
            cursor += count;
        }
        None
    }
}

impl FileBacking for NvmeBacking {
    fn read_page(&self, file_offset: u64, buf: &mut [u8; BLOCK_SIZE]) -> BlockResult {
        buf.fill(0);
        if file_offset >= self.size {
            return Ok(());
        }
        if let Some(block) = self.file_offset_to_block(file_offset) {
            // Direct disk read — bypasses block page cache.
            // File cache is the sole cache for file data.
            let mut raw = [0u8; BLOCK_SIZE];
            // `buf` is already zeroed, so a failed read leaves the caller a
            // hole rather than another file's data — and now says so in the
            // return as well as in the log, so a caller that is about to merge
            // a partial write into this page can decline instead.
            if page_cache::raw_block_read(block, &mut raw).is_err() {
                log!("file: read of block {block} failed; serving zeros");
                return Err(BlockError);
            }
            let valid = BLOCK_SIZE.min((self.size - file_offset) as usize);
            buf[..valid].copy_from_slice(&raw[..valid]);
        }
        Ok(())
    }

    fn file_size(&self) -> u64 {
        self.size
    }
}

/// File backed by initrd memory (RAM). No PageCache, no disk I/O.
pub struct InitrdBacking {
    /// Base address of the initrd in kernel virtual memory.
    initrd_base: *const u8,
    extents: Vec<Extent>,
    size: u64,
}

// Safety: initrd memory is static and immutable for the kernel's lifetime.
unsafe impl Send for InitrdBacking {}
unsafe impl Sync for InitrdBacking {}

impl InitrdBacking {
    pub fn new(initrd_base: *const u8, extents: Vec<Extent>, size: u64) -> Self {
        Self { initrd_base, extents, size }
    }

    /// Convert a file byte offset to a pointer into initrd memory.
    fn file_offset_to_ptr(&self, file_offset: u64) -> Option<*const u8> {
        let block_idx = file_offset / BLOCK_SIZE_U64;
        let off_in_block = (file_offset % BLOCK_SIZE_U64) as usize;
        let mut cursor = 0u64;
        for ext in &self.extents {
            let count = ext.block_count as u64;
            if block_idx < cursor + count {
                let initrd_block = ext.start_block + (block_idx - cursor);
                let ptr = unsafe {
                    self.initrd_base.add(initrd_block as usize * BLOCK_SIZE + off_in_block)
                };
                return Some(ptr);
            }
            cursor += count;
        }
        None
    }
}

impl FileBacking for InitrdBacking {
    /// Never `Err`: the initrd is one image already in memory, so there is no
    /// device under this to refuse.
    fn read_page(&self, file_offset: u64, buf: &mut [u8; BLOCK_SIZE]) -> BlockResult {
        buf.fill(0);
        if file_offset >= self.size {
            return Ok(());
        }
        if let Some(ptr) = self.file_offset_to_ptr(file_offset & !(BLOCK_SIZE_U64 - 1)) {
            let valid = BLOCK_SIZE.min((self.size - file_offset) as usize);
            unsafe {
                core::ptr::copy_nonoverlapping(ptr, buf.as_mut_ptr(), valid);
            }
        }
        Ok(())
    }

    fn file_size(&self) -> u64 {
        self.size
    }

    fn memory_ptr(&self, offset: u64, len: usize) -> Option<*const u8> {
        if len == 0 || offset + len as u64 > self.size { return None; }
        let start_ptr = self.file_offset_to_ptr(offset)?;
        let end_ptr = self.file_offset_to_ptr(offset + len as u64 - 1)?;
        let expected_end = unsafe { start_ptr.add(len - 1) };
        if end_ptr == expected_end {
            Some(start_ptr)
        } else {
            None
        }
    }
}
