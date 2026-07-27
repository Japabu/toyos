use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

use crate::block::{BlockDevice, DeviceId};
use crate::sync::Lock;
use crate::DirectMap;

// Separate locks: device I/O and cache data structures.
// Lock ordering: BLOCK_CACHE → BLOCK_DEV (never reversed).
static BLOCK_CACHE: Lock<Option<PageCache>> = Lock::new(None);
static BLOCK_DEV: Lock<Option<Box<dyn BlockDevice>>> = Lock::new(None);

/// Initialize the page cache, taking ownership of the block device.
pub fn init(dev: Box<dyn BlockDevice>) {
    let block_count = dev.block_count() as usize;
    let device_id = dev.device_id();
    *BLOCK_CACHE.lock() = Some(PageCache::new(block_count, device_id));
    *BLOCK_DEV.lock() = Some(dev);
}

/// Lock both cache and device for metadata operations (bcachefs btree, etc.).
/// Lock ordering: cache first, then device.
pub fn lock() -> PageCacheGuard {
    let cache = BLOCK_CACHE.lock();
    let dev = BLOCK_DEV.lock();
    PageCacheGuard { cache, dev }
}

pub struct PageCacheGuard {
    cache: crate::sync::LockGuard<'static, Option<PageCache>>,
    dev: crate::sync::LockGuard<'static, Option<Box<dyn BlockDevice>>>,
}

impl PageCacheGuard {
    pub fn cache_and_dev(&mut self) -> (&mut PageCache, &mut dyn BlockDevice) {
        let cache = self.cache.as_mut().expect("page cache not initialized");
        let dev = self.dev.as_mut().expect("block device not initialized");
        (cache, dev.as_mut())
    }

    pub fn block_count(&self) -> u64 {
        self.cache.as_ref().expect("page cache not initialized").block_count()
    }
}

impl core::ops::Deref for PageCacheGuard {
    type Target = PageCache;
    fn deref(&self) -> &PageCache { self.cache.as_ref().expect("page cache not initialized") }
}

impl core::ops::DerefMut for PageCacheGuard {
    fn deref_mut(&mut self) -> &mut PageCache { self.cache.as_mut().expect("page cache not initialized") }
}

/// Read a block directly from disk, bypassing the cache.
/// Locks only the device — no contention with metadata cache operations.
/// Used by NvmeBacking for file data reads (file cache is the sole data cache).
pub fn raw_block_read(block: u64, buf: &mut [u8; 4096]) {
    let mut dev = BLOCK_DEV.lock();
    let dev = dev.as_mut().expect("block device not initialized");
    dev.read_blocks(block, 1, buf);
}

/// Write a block directly to disk, bypassing the cache.
/// Locks only the device.
/// Used by filesystem write_page for file data writeback.
pub fn raw_block_write(block: u64, buf: &[u8; 4096]) {
    let mut dev = BLOCK_DEV.lock();
    let dev = dev.as_mut().expect("block device not initialized");
    dev.write_blocks(block, 1, buf);
}

/// Flush the block device write buffer.
pub fn raw_block_flush() {
    let mut dev = BLOCK_DEV.lock();
    let dev = dev.as_mut().expect("block device not initialized");
    dev.flush();
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

    pub fn phys_addr(&self, block: u64) -> Option<DirectMap> {
        let slot = self.block_to_slot[block as usize];
        if slot == NOT_CACHED { return None; }
        Some(DirectMap::from_ptr(self.slot_data(slot).as_ptr() as *const u8))
    }

    pub fn ensure_cached(&mut self, dev: &mut dyn BlockDevice, block: u64) -> DirectMap {
        self.read(dev, block);
        self.phys_addr(block).unwrap()
    }

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

    pub fn prefetch(&mut self, dev: &mut dyn BlockDevice, blocks: &[u64]) {
        let mut i = 0;
        while i < blocks.len() {
            if self.block_to_slot[blocks[i] as usize] != NOT_CACHED {
                i += 1;
                continue;
            }

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

            let first_slot = self.next_slot;
            for j in 0..run_len {
                self.alloc_slot(first_block + j as u64);
            }

            let first_chunk = first_slot as usize / PAGES_PER_CHUNK;
            let last_chunk = (first_slot as usize + run_len - 1) / PAGES_PER_CHUNK;

            if first_chunk == last_chunk {
                let page_in_chunk = first_slot as usize % PAGES_PER_CHUNK;
                let off = page_in_chunk * 4096;
                let end = off + run_len * 4096;
                let buf = &mut self.chunks[first_chunk][off..end];
                dev.read_blocks(first_block, run_len as u32, buf);
            } else {
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
        self.slot_data_mut(slot)
    }

    pub fn sync(&mut self, dev: &mut dyn BlockDevice) {
        let mut dirty_blocks: Vec<u64> = (0..self.next_slot)
            .filter(|&s| self.dirty[s as usize])
            .map(|s| self.slot_to_block[s as usize])
            .collect();

        if dirty_blocks.is_empty() {
            return;
        }
        dirty_blocks.sort_unstable();

        let mut buf = vec![0u8; 32 * 4096];
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

            for j in 0..count {
                let block = start + j as u64;
                let slot = self.block_to_slot[block as usize];
                let page = self.slot_data(slot);
                buf[j as usize * 4096..(j as usize + 1) * 4096].copy_from_slice(page);
            }

            dev.write_blocks(start, count, &buf[..count as usize * 4096]);

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
