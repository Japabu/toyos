use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use hashbrown::HashMap;

use crate::block::{self, BlockDevice, BlockError, BlockResult, DeviceId};
use crate::sync::Lock;

// Separate locks: device I/O and cache data structures.
// Lock ordering: BLOCK_CACHE → BLOCK_DEV (never reversed).
static BLOCK_CACHE: Lock<Option<PageCache>> = Lock::new(None);
static BLOCK_DEV: Lock<Option<Box<dyn BlockDevice>>> = Lock::new(None);

/// Initialize the page cache, taking ownership of the block device.
pub fn init(dev: Box<dyn BlockDevice>) {
    let block_count = dev.block_count();
    let cache = PageCache::new(block_count, dev.device_id());
    log!("page cache: {} device blocks, index sized for {} cached blocks, cap {} slots",
        block_count, cache.index_capacity(), cache.max_slots);
    *BLOCK_CACHE.lock() = Some(cache);
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
#[must_use = "a failed read leaves the buffer holding whatever it held before"]
pub fn raw_block_read(block: u64, buf: &mut [u8; 4096]) -> BlockResult {
    let mut dev = BLOCK_DEV.lock();
    let dev = dev.as_mut().expect("block device not initialized");
    dev.read_blocks(block, 1, buf)
}

/// Write a block directly to disk, bypassing the cache.
/// Locks only the device.
/// Used by filesystem write_page for file data writeback.
#[must_use = "a failed write did not reach the device"]
pub fn raw_block_write(block: u64, buf: &[u8; 4096]) -> BlockResult {
    let mut dev = BLOCK_DEV.lock();
    let dev = dev.as_mut().expect("block device not initialized");
    dev.write_blocks(block, 1, buf)
}

/// The block number of a slot that names nothing — see [`PageCache::unbind`].
/// No device can have this block: `block_count` is a byte count over 4096.
const NO_BLOCK: u64 = u64::MAX;

/// Pages per chunk. 256 pages = 1MB per chunk allocation.
const PAGES_PER_CHUNK: usize = 256;
const CHUNK_SIZE: usize = PAGES_PER_CHUNK * 4096;

pub struct PageCache {
    /// Maps block number → slot index. Keyed by the cached set, never sized
    /// by the device: one index entry per *device* block costs 4 bytes of
    /// heap per KiB of disk, which a 244 GB laptop NVMe turns into a 238 MB
    /// request the object allocator refuses outright.
    block_to_slot: HashMap<u64, u32>,
    /// Maps slot index → block number (for sync and for eviction, which has
    /// to un-index the block it is taking the slot from).
    slot_to_block: Vec<u64>,
    dirty: Vec<bool>,
    /// CLOCK's second-chance bit: set on every hit, cleared when the hand
    /// passes. Without it a full cache degenerates to FIFO and evicts the
    /// superblock — touched by every btree walk — as readily as a leaf.
    referenced: Vec<bool>,
    /// Page data stored in fixed-size 1MB chunks to avoid giant reallocations.
    ///
    /// `Box<[u8]>` and not `Box<[u8; CHUNK_SIZE]>`: the array type forced a
    /// hand-written `alloc_zeroed` + `Box::from_raw`, because `Box::new` of a
    /// 1 MiB array builds it on the kernel stack first. Every use of a chunk
    /// is a `[off..off + 4096]` slice, which the unsized type serves
    /// identically — and `vec![0u8; CHUNK_SIZE]` reaches the same
    /// `alloc_zeroed` through `alloc`'s own zeroing specialization for `u8`,
    /// with no stack temporary and no `unsafe`.
    chunks: Vec<Box<[u8]>>,
    hand: u32,
    max_slots: usize,
    evictions: u64,
    /// The device's size, which the filesystem needs. Nothing in here may
    /// size an allocation by it.
    block_count: u64,
    _device_id: DeviceId,
}

impl PageCache {
    fn new(block_count: u64, device_id: DeviceId) -> Self {
        let max_slots = block::metadata_cache_blocks();
        Self {
            // Reserved up front so `index_capacity` reports the real ceiling
            // from the first boot line rather than after the workload has
            // grown into it.
            block_to_slot: HashMap::with_capacity(max_slots),
            slot_to_block: Vec::with_capacity(max_slots),
            dirty: Vec::with_capacity(max_slots),
            referenced: Vec::with_capacity(max_slots),
            chunks: Vec::with_capacity(max_slots.div_ceil(PAGES_PER_CHUNK)),
            hand: 0,
            max_slots,
            evictions: 0,
            block_count,
            _device_id: device_id,
        }
    }

    pub fn block_count(&self) -> u64 {
        self.block_count
    }

    /// Blocks the index has room for. A value anywhere near `block_count`
    /// means someone sized the index by the device again — which is what the
    /// boot line reports and `nvme_large_device` asserts against.
    pub fn index_capacity(&self) -> usize {
        self.block_to_slot.capacity()
    }

    /// A slot bound to `block`, or `None` when nothing could be freed for it.
    ///
    /// `None` is reachable only when every resident slot is dirty *and* the
    /// write-back that would clean them failed, which is a device error and
    /// not the fail-fast this used to be.
    fn alloc_slot(&mut self, dev: &mut dyn BlockDevice, block: u64) -> Option<u32> {
        let slot = if self.slot_to_block.len() < self.max_slots {
            let slot = self.slot_to_block.len() as u32;
            self.slot_to_block.push(block);
            self.dirty.push(false);
            self.referenced.push(false);
            if slot as usize / PAGES_PER_CHUNK >= self.chunks.len() {
                self.chunks.push(vec![0u8; CHUNK_SIZE].into_boxed_slice());
            }
            slot
        } else {
            let slot = self.take_victim(dev)?;
            self.block_to_slot.remove(&self.slot_to_block[slot as usize]);
            self.slot_to_block[slot as usize] = block;
            self.dirty[slot as usize] = false;
            self.evictions += 1;
            // One line per full turnover of the cache, so the series scales
            // with the bound instead of with a number picked here: it is the
            // only evidence from outside the kernel that residency stays flat
            // while the eviction count climbs.
            if self.evictions == 1 || self.evictions.is_multiple_of(self.max_slots as u64) {
                log!("page cache: {} evictions, {}/{} slots resident",
                    self.evictions, self.slot_to_block.len(), self.max_slots);
            }
            slot
        };
        self.referenced[slot as usize] = true;
        self.block_to_slot.insert(block, slot);
        Some(slot)
    }

    /// Undo a binding whose fill never happened.
    ///
    /// The slot still holds the evicted block's bytes, so the one thing that
    /// must not survive is the *label*. With the index entry gone and the slot
    /// naming nothing, the next reader misses and asks the device again
    /// instead of being handed the previous tenant's data under the new
    /// number — which is what a discarded read status used to produce, and it
    /// parses, because it is a real block.
    fn unbind(&mut self, slot: u32, block: u64) {
        self.block_to_slot.remove(&block);
        self.slot_to_block[slot as usize] = NO_BLOCK;
        self.dirty[slot as usize] = false;
        self.referenced[slot as usize] = false;
    }

    /// Free a slot for reuse, writing back first if that is what it takes.
    ///
    /// Writing a dirty metadata block back early is not a new hazard: the
    /// filesystem has no journal and `sync` already commits every dirty slot
    /// in whatever order the block numbers fall, so eviction can only reorder
    /// writes that were never ordered.
    fn take_victim(&mut self, dev: &mut dyn BlockDevice) -> Option<u32> {
        if let Some(slot) = self.clock_pick() {
            return Some(slot);
        }
        // Every resident block is dirty. One coalesced write-back is the same
        // work unmount does and turns the whole cache clean, so the next scan
        // cannot fail — unless the device refused the write-back, in which
        // case the slots that stayed dirty are the ones this cannot have.
        if self.sync(dev).is_err() {
            log!("page cache: write-back failed; no slot could be freed");
        }
        self.clock_pick()
    }

    /// CLOCK second chance over clean slots. Two revolutions: the first can
    /// spend clearing reference bits, the second then finds an unreferenced
    /// slot unless every one of them is dirty.
    fn clock_pick(&mut self) -> Option<u32> {
        let n = self.slot_to_block.len() as u32;
        for _ in 0..2 * n {
            let slot = self.hand as usize;
            self.hand = if self.hand + 1 == n { 0 } else { self.hand + 1 };
            if self.dirty[slot] {
                continue;
            }
            if self.referenced[slot] {
                self.referenced[slot] = false;
                continue;
            }
            return Some(slot as u32);
        }
        None
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

    fn slot_of(&self, block: u64) -> Option<u32> {
        self.block_to_slot.get(&block).copied()
    }

    pub fn read(&mut self, dev: &mut dyn BlockDevice, block: u64) -> Result<&[u8], BlockError> {
        if let Some(slot) = self.slot_of(block) {
            self.referenced[slot as usize] = true;
            return Ok(self.slot_data(slot));
        }
        let slot = self.alloc_slot(dev, block).ok_or(BlockError::Device)?;
        let page = self.slot_data_mut(slot);
        if let Err(e) = dev.read_blocks(block, 1, page) {
            self.unbind(slot, block);
            return Err(e);
        }
        Ok(self.slot_data(slot))
    }

    pub fn write_new(
        &mut self,
        dev: &mut dyn BlockDevice,
        block: u64,
    ) -> Result<&mut [u8], BlockError> {
        let slot = match self.slot_of(block) {
            Some(slot) => {
                self.referenced[slot as usize] = true;
                slot
            }
            None => self.alloc_slot(dev, block).ok_or(BlockError::Device)?,
        };
        self.dirty[slot as usize] = true;
        let page = self.slot_data_mut(slot);
        // A reused slot still holds the evicted block's bytes, and the caller
        // is entitled to a blank block.
        page.fill(0);
        Ok(page)
    }

    /// Write every dirty slot back, coalescing runs of consecutive blocks.
    ///
    /// The run walks slots rather than block numbers, so the write-back needs
    /// no index lookups at all — `slot_to_block` is the direction this pass
    /// wants, and the index is the wrong way round for it.
    pub fn sync(&mut self, dev: &mut dyn BlockDevice) -> BlockResult {
        let mut pending: Vec<u32> = (0..self.slot_to_block.len() as u32)
            .filter(|&s| self.dirty[s as usize])
            .collect();

        if pending.is_empty() {
            return Ok(());
        }
        pending.sort_unstable_by_key(|&s| self.slot_to_block[s as usize]);

        let mut buf = vec![0u8; 32 * 4096];
        // **The worst of the parts, not the first.** One write-back is many
        // runs plus a flush, and a caller told "your budget expired" for a
        // composite that also contained a run the device refused would ask
        // again for a write it can never make. `BlockError::worse` is where
        // that rule is stated.
        let mut failed: Option<BlockError> = None;
        let mut i = 0;
        while i < pending.len() {
            let start = self.slot_to_block[pending[i] as usize];
            let mut count = 1usize;

            while i + count < pending.len()
                && self.slot_to_block[pending[i + count] as usize] == start + count as u64
                && count < 32
            {
                count += 1;
            }

            for j in 0..count {
                let page = self.slot_data(pending[i + j]);
                buf[j * 4096..(j + 1) * 4096].copy_from_slice(page);
            }

            // A run that did not land stays dirty, so a later sync tries it
            // again and `take_victim` will not hand the slot to another block.
            // Carrying on with the remaining runs is deliberate: one bad run
            // is not a reason to leave the rest of the cache unwritten.
            match dev.write_blocks(start, count as u32, &buf[..count * 4096]) {
                Ok(()) => {
                    for j in 0..count {
                        self.dirty[pending[i + j] as usize] = false;
                    }
                }
                Err(e) => {
                    log!("page cache: write-back of {count} blocks at {start} failed");
                    failed = Some(failed.map_or(e, |had| had.worse(e)));
                }
            }

            i += count;
        }

        if let Err(e) = dev.flush() {
            log!("page cache: flush failed; the write-back above is not durable");
            failed = Some(failed.map_or(e, |had| had.worse(e)));
        }
        failed.map_or(Ok(()), Err)
    }
}
