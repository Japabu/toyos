use crate::block_io::{BlockBuf, BlockNum, BlockIO, BLOCK_SIZE};
use crate::fs::FsError;

const BITS_PER_BLOCK: u64 = (BLOCK_SIZE * 8) as u64;

/// A run of blocks the allocator actually reserved.
///
/// `len` is what you got, never what you asked for. The pair this replaced was
/// `(BlockNum, u32)`, which reads as "here is your block, and how many" and was
/// destructured positionally at three sites; one of them then addressed a block
/// past the end of the short run it had just recorded, so a sparse write on a
/// fragmented volume landed on another file. A struct cannot be read as "all
/// of it" by accident.
#[must_use]
#[derive(Debug, Clone, Copy)]
pub struct Run {
    pub start: BlockNum,
    pub len: u32,
}

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
        Ok(self.alloc_exact(io, 1)?.start)
    }

    /// Reserve as much of `wanted` as one contiguous run can cover.
    ///
    /// The run is never empty and may be shorter than asked for, so every
    /// caller has to loop or has to be wrong.
    pub fn alloc_up_to(&mut self, io: &dyn BlockIO, wanted: u32) -> Result<Run, FsError> {
        // A zero-length run would let a caller's loop spin without progress.
        let wanted = wanted.max(1);
        let (start, len) = self.longest_free_run(io, wanted)?;
        Ok(self.reserve(io, start, len.min(wanted)))
    }

    /// Reserve all of `count` or nothing, for callers that cannot place a
    /// short run. Nothing is marked used unless the whole run is there.
    pub fn alloc_exact(&mut self, io: &dyn BlockIO, count: u32) -> Result<Run, FsError> {
        let (start, len) = self.longest_free_run(io, count)?;
        if len < count {
            return Err(FsError::NoSpace {
                requested: count,
                available: self.free_blocks,
            });
        }
        Ok(self.reserve(io, start, count))
    }

    /// Mark a run used and move the cursor past it.
    fn reserve(&mut self, io: &dyn BlockIO, start: u64, len: u32) -> Run {
        let start_block = BlockNum::new(start);
        self.set_range_used(io, start_block, len as u64);
        self.free_blocks -= len as u64;
        self.next_alloc = start + len as u64;
        if self.next_alloc >= self.total_blocks {
            self.next_alloc = 0;
        }
        Run { start: start_block, len }
    }

    /// The longest free run found scanning from the `next_alloc` cursor,
    /// wrapping once, stopping early once `wanted` blocks are in hand.
    fn longest_free_run(&self, io: &dyn BlockIO, wanted: u32) -> Result<(u64, u32), FsError> {
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

        Ok((start, best_count))
    }

    /// Free a contiguous range of blocks.
    pub fn free_range(&mut self, io: &dyn BlockIO, start: BlockNum, count: u32) {
        for i in 0..count as u64 {
            self.set_free(io, BlockNum::new(start.raw() + i));
        }
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
