use crate::block_io::{BlockBuf, BlockNum, BlockIO, BLOCK_SIZE};
use crate::fs::FsError;

const BITS_PER_BLOCK: u64 = (BLOCK_SIZE * 8) as u64;

/// Bitmap-based block allocator.
///
/// The bitmap is stored on disk starting at `bitmap_start` and spanning
/// `bitmap_blocks` blocks. Each bit represents one block: 1 = used, 0 = free.
pub struct BitmapAllocator {
    pub bitmap_start: BlockNum,
    pub bitmap_blocks: u64,
    pub total_blocks: u64,
    pub free_blocks: u64,
    pub next_alloc: u64, // cursor — scan starts here, wraps once
}

impl BitmapAllocator {
    /// Mark a single block as used in the bitmap.
    pub fn set_used(&self, io: &dyn BlockIO, block: BlockNum) {
        let bit = block.raw();
        let byte_idx = bit / 8;
        let bit_idx = bit % 8;
        let bitmap_block = BlockNum::new(self.bitmap_start.raw() + byte_idx / BLOCK_SIZE as u64);
        let byte_off = (byte_idx % BLOCK_SIZE as u64) as usize;

        let mut buf = BlockBuf::zeroed();
        io.read_block(bitmap_block, &mut buf);
        buf.0[byte_off] |= 1 << bit_idx;
        io.write_block(bitmap_block, &buf);
    }

    /// Mark a single block as free in the bitmap.
    pub fn set_free(&mut self, io: &dyn BlockIO, block: BlockNum) {
        let bit = block.raw();
        let byte_idx = bit / 8;
        let bit_idx = bit % 8;
        let bitmap_block = BlockNum::new(self.bitmap_start.raw() + byte_idx / BLOCK_SIZE as u64);
        let byte_off = (byte_idx % BLOCK_SIZE as u64) as usize;

        let mut buf = BlockBuf::zeroed();
        io.read_block(bitmap_block, &mut buf);
        buf.0[byte_off] &= !(1 << bit_idx);
        io.write_block(bitmap_block, &buf);

        self.free_blocks += 1;
        if block.raw() < self.next_alloc {
            self.next_alloc = block.raw();
        }
    }

    /// Mark a contiguous range of blocks as used.
    pub fn set_range_used(&self, io: &dyn BlockIO, start: BlockNum, count: u64) {
        for i in 0..count {
            self.set_used(io, BlockNum::new(start.raw() + i));
        }
    }

    /// Check if a specific block is free.
    fn is_free(&self, io: &dyn BlockIO, block: u64) -> bool {
        if block >= self.total_blocks {
            return false;
        }
        let byte_idx = block / 8;
        let bit_idx = block % 8;
        let bitmap_block = BlockNum::new(self.bitmap_start.raw() + byte_idx / BLOCK_SIZE as u64);
        let byte_off = (byte_idx % BLOCK_SIZE as u64) as usize;

        let mut buf = BlockBuf::zeroed();
        io.read_block(bitmap_block, &mut buf);
        (buf.0[byte_off] >> bit_idx) & 1 == 0
    }

    /// Allocate a single block.
    pub fn alloc_block(&mut self, io: &dyn BlockIO) -> Result<BlockNum, FsError> {
        match self.alloc_contiguous(io, 1)? {
            (block, 1) => Ok(block),
            _ => unreachable!(),
        }
    }

    /// Try to allocate up to `wanted` contiguous blocks.
    ///
    /// Returns (start_block, actual_count) where actual_count >= 1.
    /// Scans from `next_alloc` cursor, wrapping once around the bitmap.
    pub fn alloc_contiguous(
        &mut self,
        io: &dyn BlockIO,
        wanted: u32,
    ) -> Result<(BlockNum, u32), FsError> {
        if self.free_blocks == 0 {
            return Err(FsError::NoSpace {
                requested: wanted,
                available: 0,
            });
        }

        let total = self.total_blocks;
        let start_pos = self.next_alloc;
        let mut best_start = None;
        let mut best_count = 0u32;

        // Scan from cursor, wrap once
        let mut pos = start_pos;
        let mut wrapped = false;
        let mut run_start = None;
        let mut run_count = 0u32;

        // Cache the current bitmap block to avoid re-reading for every bit
        let mut cached_bitmap_block = u64::MAX;
        let mut cached_buf = BlockBuf::zeroed();

        loop {
            if wrapped && pos >= start_pos {
                break;
            }
            if pos >= total {
                if wrapped {
                    break;
                }
                wrapped = true;
                pos = 0;
                run_start = None;
                run_count = 0;
                continue;
            }

            // Read bitmap block if not cached
            let byte_idx = pos / 8;
            let bblock = self.bitmap_start.raw() + byte_idx / BLOCK_SIZE as u64;
            if bblock != cached_bitmap_block {
                io.read_block(BlockNum::new(bblock), &mut cached_buf);
                cached_bitmap_block = bblock;
            }

            let byte_off = (byte_idx % BLOCK_SIZE as u64) as usize;
            let bit_idx = pos % 8;
            let is_free = (cached_buf.0[byte_off] >> bit_idx) & 1 == 0;

            if is_free {
                if run_start.is_none() {
                    run_start = Some(pos);
                    run_count = 0;
                }
                run_count += 1;

                if run_count >= wanted {
                    // Found exactly what we wanted
                    best_start = run_start;
                    best_count = run_count;
                    break;
                }

                if run_count > best_count {
                    best_start = run_start;
                    best_count = run_count;
                }
            } else {
                run_start = None;
                run_count = 0;
            }

            pos += 1;
        }

        let start = best_start.ok_or(FsError::NoSpace {
            requested: wanted,
            available: self.free_blocks,
        })?;

        let count = best_count.min(wanted);
        let start_block = BlockNum::new(start);

        // Mark allocated blocks as used
        self.set_range_used(io, start_block, count as u64);
        self.free_blocks -= count as u64;
        self.next_alloc = start + count as u64;
        if self.next_alloc >= total {
            self.next_alloc = 0;
        }

        Ok((start_block, count))
    }

    /// Free a contiguous range of blocks.
    pub fn free_range(&mut self, io: &dyn BlockIO, start: BlockNum, count: u32) {
        for i in 0..count as u64 {
            self.set_free(io, BlockNum::new(start.raw() + i));
        }
        // free_blocks already incremented by set_free
        // Adjust: set_free increments per call, but we want the total
        // Actually set_free handles it correctly — each call increments by 1
    }

    /// Initialize bitmap on disk: zero all bitmap blocks, then mark metadata blocks as used.
    pub fn format(
        io: &dyn BlockIO,
        bitmap_start: BlockNum,
        bitmap_blocks: u64,
        total_blocks: u64,
        metadata_blocks: u64,
    ) -> Self {
        // Zero all bitmap blocks
        let zero = BlockBuf::zeroed();
        for i in 0..bitmap_blocks {
            io.write_block(BlockNum::new(bitmap_start.raw() + i), &zero);
        }

        let mut alloc = Self {
            bitmap_start,
            bitmap_blocks,
            total_blocks,
            free_blocks: total_blocks - metadata_blocks,
            next_alloc: metadata_blocks,
        };

        // Mark metadata blocks (superblock, bitmap, journal area) as used
        for i in 0..metadata_blocks {
            alloc.set_used(io, BlockNum::new(i));
        }

        // Also mark the last block (superblock backup) as used
        alloc.set_used(io, BlockNum::new(total_blocks - 1));
        alloc.free_blocks -= 1; // account for backup block

        alloc
    }
}
