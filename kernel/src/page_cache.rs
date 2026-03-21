use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

use crate::block::{BlockDevice, DeviceId};
use crate::sync::Lock;
use crate::DirectMap;

struct PageCacheWithDev {
    cache: PageCache,
    dev: Box<dyn BlockDevice>,
}

static PAGE_CACHE: Lock<Option<PageCacheWithDev>> = Lock::new(None);

/// Initialize the page cache, taking ownership of the block device.
pub fn init(dev: Box<dyn BlockDevice>) {
    let block_count = dev.block_count() as usize;
    let device_id = dev.device_id();
    *PAGE_CACHE.lock() = Some(PageCacheWithDev {
        cache: PageCache::new(block_count, device_id),
        dev,
    });
}

pub fn lock() -> PageCacheGuard {
    PageCacheGuard(PAGE_CACHE.lock())
}

pub struct PageCacheGuard(crate::sync::LockGuard<'static, Option<PageCacheWithDev>>);

impl PageCacheGuard {
    pub fn cache_and_dev(&mut self) -> (&mut PageCache, &mut dyn BlockDevice) {
        let inner = self.0.as_mut().expect("page cache not initialized");
        (&mut inner.cache, inner.dev.as_mut())
    }
}

impl core::ops::Deref for PageCacheGuard {
    type Target = PageCache;
    fn deref(&self) -> &PageCache { &self.0.as_ref().expect("page cache not initialized").cache }
}

impl core::ops::DerefMut for PageCacheGuard {
    fn deref_mut(&mut self) -> &mut PageCache { &mut self.0.as_mut().expect("page cache not initialized").cache }
}

const NOT_CACHED: u32 = u32::MAX;

/// Pages per chunk. 256 pages = 1MB per chunk allocation.
const PAGES_PER_CHUNK: usize = 256;
const CHUNK_SIZE: usize = PAGES_PER_CHUNK * 4096;

pub struct PageCache {
    /// Maps block number → slot index. NOT_CACHED if not in cache.
    block_to_slot: Vec<u32>,
    /// Maps slot index → block number (for sync).
    slot_to_block: Vec<u64>,
    /// Dirty flag per slot.
    dirty: Vec<bool>,
    /// Page data stored in fixed-size 1MB chunks to avoid giant reallocations.
    chunks: Vec<Box<[u8; CHUNK_SIZE]>>,
    next_slot: u32,
    _device_id: DeviceId,
}

impl PageCache {
    fn new(block_count: usize, device_id: DeviceId) -> Self {
        Self {
            block_to_slot: vec![NOT_CACHED; block_count],
            slot_to_block: Vec::with_capacity(4096),
            dirty: Vec::with_capacity(4096),
            chunks: Vec::with_capacity(64),
            next_slot: 0,
            _device_id: device_id,
        }
    }

    pub fn block_count(&self) -> u64 {
        self.block_to_slot.len() as u64
    }

    fn alloc_slot(&mut self, block: u64) -> u32 {
        let slot = self.next_slot;
        self.next_slot += 1;
        self.block_to_slot[block as usize] = slot;
        self.slot_to_block.push(block);
        self.dirty.push(false);
        // Allocate new chunk if needed
        let chunk_idx = slot as usize / PAGES_PER_CHUNK;
        if chunk_idx >= self.chunks.len() {
            let chunk: Box<[u8; CHUNK_SIZE]> = unsafe {
                let layout = alloc::alloc::Layout::new::<[u8; CHUNK_SIZE]>();
                let ptr = alloc::alloc::alloc_zeroed(layout);
                assert!(!ptr.is_null(), "page cache: chunk allocation failed");
                Box::from_raw(ptr as *mut [u8; CHUNK_SIZE])
            };
            self.chunks.push(chunk);
        }
        slot
    }

    fn slot_data(&self, slot: u32) -> &[u8] {
        let chunk_idx = slot as usize / PAGES_PER_CHUNK;
        let page_in_chunk = slot as usize % PAGES_PER_CHUNK;
        let off = page_in_chunk * 4096;
        &self.chunks[chunk_idx][off..off + 4096]
    }

    fn slot_data_mut(&mut self, slot: u32) -> &mut [u8] {
        let chunk_idx = slot as usize / PAGES_PER_CHUNK;
        let page_in_chunk = slot as usize % PAGES_PER_CHUNK;
        let off = page_in_chunk * 4096;
        &mut self.chunks[chunk_idx][off..off + 4096]
    }

    /// Get the physical address of a cached block's 4KB page.
    /// Identity mapping: virtual pointer IS physical address.
    pub fn phys_addr(&self, block: u64) -> Option<DirectMap> {
        let slot = self.block_to_slot[block as usize];
        if slot == NOT_CACHED { return None; }
        Some(DirectMap::from_ptr(self.slot_data(slot).as_ptr() as *const u8))
    }

    /// Ensure a block is cached (loading from device if needed) and return its physical address.
    pub fn ensure_cached(&mut self, dev: &mut dyn BlockDevice, block: u64) -> DirectMap {
        self.read(dev, block);
        self.phys_addr(block).unwrap()
    }

    /// Read a block, returning a reference to the cached 4KB page.
    /// Loads from device on cache miss.
    pub fn read(&mut self, dev: &mut dyn BlockDevice, block: u64) -> &[u8] {
        let slot = self.block_to_slot[block as usize];
        if slot != NOT_CACHED {
            return self.slot_data(slot);
        }
        let slot = self.alloc_slot(block);
        let page = self.slot_data_mut(slot);
        dev.read_blocks(block, 1, page);
        self.dirty[slot as usize] = false;
        self.slot_data(slot)
    }

    /// Prefetch a contiguous range of blocks into the cache with batched I/O.
    /// Skips blocks already cached. Reads up to 32 blocks per device call.
    pub fn prefetch(&mut self, dev: &mut dyn BlockDevice, blocks: &[u64]) {
        // Find runs of contiguous uncached blocks and batch-read them.
        let mut i = 0;
        while i < blocks.len() {
            // Skip already-cached blocks
            if self.block_to_slot[blocks[i] as usize] != NOT_CACHED {
                i += 1;
                continue;
            }

            // Start a contiguous run of uncached blocks
            let run_start = i;
            let first_block = blocks[i];
            i += 1;
            while i < blocks.len()
                && i - run_start < 32
                && blocks[i] == first_block + (i - run_start) as u64
                && self.block_to_slot[blocks[i] as usize] == NOT_CACHED
            {
                i += 1;
            }
            let run_len = i - run_start;

            // Allocate slots for the entire run
            let first_slot = self.next_slot;
            for j in 0..run_len {
                self.alloc_slot(first_block + j as u64);
            }

            // Check if all slots land in the same chunk (common case for sequential alloc)
            let first_chunk = first_slot as usize / PAGES_PER_CHUNK;
            let last_chunk = (first_slot as usize + run_len - 1) / PAGES_PER_CHUNK;

            if first_chunk == last_chunk {
                // All pages are contiguous in memory — read directly into chunk
                let page_in_chunk = first_slot as usize % PAGES_PER_CHUNK;
                let off = page_in_chunk * 4096;
                let end = off + run_len * 4096;
                let buf = &mut self.chunks[first_chunk][off..end];
                dev.read_blocks(first_block, run_len as u32, buf);
            } else {
                // Pages span chunks — read into temp buffer, then scatter
                let mut buf = vec![0u8; run_len * 4096];
                dev.read_blocks(first_block, run_len as u32, &mut buf);
                for j in 0..run_len {
                    let slot = first_slot + j as u32;
                    let page = self.slot_data_mut(slot);
                    page.copy_from_slice(&buf[j * 4096..(j + 1) * 4096]);
                }
            }
        }
    }

    /// Get a mutable reference to a cached block for modification.
    /// Loads from device on cache miss, marks page dirty.
    pub fn write(&mut self, dev: &mut dyn BlockDevice, block: u64) -> &mut [u8] {
        let slot = self.block_to_slot[block as usize];
        if slot != NOT_CACHED {
            self.dirty[slot as usize] = true;
            return self.slot_data_mut(slot);
        }
        let slot = self.alloc_slot(block);
        let page = self.slot_data_mut(slot);
        dev.read_blocks(block, 1, page);
        self.dirty[slot as usize] = true;
        self.slot_data_mut(slot)
    }

    /// Get a mutable reference to a block without reading from device first.
    /// Use when the entire block will be overwritten.
    pub fn write_new(&mut self, _dev: &mut dyn BlockDevice, block: u64) -> &mut [u8] {
        let slot = self.block_to_slot[block as usize];
        if slot != NOT_CACHED {
            self.dirty[slot as usize] = true;
            let page = self.slot_data_mut(slot);
            page.fill(0);
            return page;
        }
        let slot = self.alloc_slot(block);
        self.dirty[slot as usize] = true;
        // Chunk memory is already zeroed on allocation
        self.slot_data_mut(slot)
    }

    /// Flush all dirty pages for a device to disk.
    /// Batches contiguous runs into multi-block writes for performance.
    pub fn sync(&mut self, dev: &mut dyn BlockDevice) {
        // Collect dirty block numbers, sorted for batching
        let mut dirty_blocks: Vec<u64> = (0..self.next_slot)
            .filter(|&s| self.dirty[s as usize])
            .map(|s| self.slot_to_block[s as usize])
            .collect();

        if dirty_blocks.is_empty() {
            return;
        }
        dirty_blocks.sort_unstable();

        // Batch contiguous runs and write together
        let mut buf = vec![0u8; 32 * 4096]; // reuse buffer across batches
        let mut i = 0;
        while i < dirty_blocks.len() {
            let start = dirty_blocks[i];
            let mut count = 1u32;

            while i + (count as usize) < dirty_blocks.len()
                && dirty_blocks[i + (count as usize)] == start + count as u64
                && count < 32
            {
                count += 1;
            }

            // Copy cached pages into contiguous buffer
            for j in 0..count {
                let block = start + j as u64;
                let slot = self.block_to_slot[block as usize];
                let page = self.slot_data(slot);
                buf[j as usize * 4096..(j as usize + 1) * 4096].copy_from_slice(page);
            }

            dev.write_blocks(start, count, &buf[..count as usize * 4096]);

            // Mark pages clean
            for j in 0..count {
                let block = start + j as u64;
                let slot = self.block_to_slot[block as usize];
                self.dirty[slot as usize] = false;
            }

            i += count as usize;
        }

        dev.flush();
    }
}
