use alloc::sync::Arc;
use alloc::vec::Vec;

use bcachefs::Extent;
use crate::block::{BlockError, BlockResult};
use crate::page_cache;
use crate::sync::Lock;

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
}

/// Which blocks a `/home` file's data lives in, and whether they are still
/// that file's.
///
/// Every [`NvmeBacking`] for one name reads through the same one of these
/// rather than a copy taken at open, so unlinking the file is a single store
/// that every outstanding backing sees — the one in a running process's
/// address space, the one the file cache re-fetches evicted pages through,
/// and any handed out since.
///
/// It keeps nothing alive. bcachefs's allocator has the blocks back the moment
/// the entry is gone and the next file takes them, which is exactly why a read
/// after that has to *fail*: the blocks are still readable and what is in them
/// belongs to somebody else. Refcounting the blocks — keeping a deleted file's
/// data alive for as long as something can read it — is the POSIX answer to a
/// question ToyOS has not been asked, and it would need
/// `specs/capability-handles-spec.md` to be honest about it.
pub struct FileBlocks {
    /// `None` once the filesystem has taken the blocks back.
    extents: Lock<Option<Vec<Extent>>>,
}

impl FileBlocks {
    pub fn new(extents: Vec<Extent>) -> Arc<Self> {
        Arc::new(Self { extents: Lock::new(Some(extents)) })
    }

    /// Give the blocks up. Every read through every backing that shares this
    /// fails from here on.
    pub fn revoke(&self) {
        *self.extents.lock() = None;
    }

    /// Run `f` over the current extent list, or `None` if the file is gone.
    ///
    /// The lock is held across `f` on purpose: the write path resolves and
    /// allocates inside it, and an extent list read between the resolve and
    /// the record would be one the file does not have yet.
    pub fn with<R>(&self, f: impl FnOnce(&mut Vec<Extent>) -> R) -> Option<R> {
        self.extents.lock().as_mut().map(f)
    }
}

/// The block holding `file_offset`, if the extents reach that far.
fn offset_to_block(extents: &[Extent], file_offset: u64) -> Option<u64> {
    let block_idx = file_offset / BLOCK_SIZE_U64;
    let mut cursor = 0u64;
    for ext in extents {
        let count = ext.block_count as u64;
        if block_idx < cursor + count {
            return Some(ext.start_block + (block_idx - cursor));
        }
        cursor += count;
    }
    None
}

/// File backed by NVMe blocks via the kernel PageCache.
pub struct NvmeBacking {
    blocks: Arc<FileBlocks>,
    size: u64,
}

impl NvmeBacking {
    pub fn new(blocks: Arc<FileBlocks>, size: u64) -> Self {
        Self { blocks, size }
    }
}

impl FileBacking for NvmeBacking {
    fn read_page(&self, file_offset: u64, buf: &mut [u8; BLOCK_SIZE]) -> BlockResult {
        buf.fill(0);
        if file_offset >= self.size {
            return Ok(());
        }
        // A backing whose file has been unlinked names blocks the allocator
        // has already handed to somebody else. Reading them would serve
        // another file's contents to whoever still holds this mapping.
        let Some(block) = self.blocks.with(|extents| offset_to_block(extents, file_offset)) else {
            log!("file: read through a backing whose file was deleted");
            return Err(BlockError);
        };
        if let Some(block) = block {
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
}
