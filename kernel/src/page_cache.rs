use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

use crate::block::{BlockDevice, DeviceId};
use crate::sync::Lock;

static PAGE_CACHE: Lock<Option<PageCache>> = Lock::new(None);

/// Initialize the page cache for a specific block device.
pub fn init(dev: &dyn BlockDevice) {
    let block_count = dev.block_count() as usize;
    *PAGE_CACHE.lock() = Some(PageCache::new(block_count, dev.device_id()));
}

pub fn lock() -> PageCacheGuard {
    PageCacheGuard(PAGE_CACHE.lock())
}

pub struct PageCacheGuard(crate::sync::LockGuard<'static, Option<PageCache>>);

impl core::ops::Deref for PageCacheGuard {
    type Target = PageCache;
    fn deref(&self) -> &PageCache { self.0.as_ref().expect("page cache not initialized") }
}

impl core::ops::DerefMut for PageCacheGuard {
    fn deref_mut(&mut self) -> &mut PageCache { self.0.as_mut().expect("page cache not initialized") }
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

    fn alloc_slot(&mut self, block: u64) -> u32 {
        let slot = self.next_slot;
        self.next_slot += 1;
        self.block_to_slot[block as usize] = slot;
        self.slot_to_block.push(block);
        self.dirty.push(false);
        // Allocate new chunk if needed
        let chunk_idx = slot as usize / PAGES_PER_CHUNK;
        if chunk_idx >= self.chunks.len() {
            self.chunks.push(Box::new([0u8; CHUNK_SIZE]));
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
