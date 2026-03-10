use alloc::borrow::Cow;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cmp;

use crate::block::BlockDevice;
use crate::page_cache::PageCache;
use crate::vfs::FileSystem;

const MAGIC: [u8; 4] = *b"TOYF";
const VERSION: u32 = 1;
const BLOCK_SIZE: u32 = 4096;

// Entry layout (256 bytes, 16 per block):
//   [0]        type: 0=free, 1=file, 2=symlink
//   [4..8]     name_len: u32
//   [8..16]    size: u64
//   [16..24]   mtime: u64
//   [24..88]   direct[0..7]: 8 × u64 block pointers
//   [88..96]   indirect: u64 block pointer (512 entries = +2MB)
//   [96..104]  double_indirect: u64 block pointer (512×512 = +1GB)
//   [104..256] name: [u8; 152]
const ENTRY_SIZE: usize = 256;
const ENTRIES_PER_BLOCK: usize = BLOCK_SIZE as usize / ENTRY_SIZE;
const NAME_MAX: usize = 152;
const DIRECT_BLOCKS: usize = 8;
const PTRS_PER_INDIRECT: usize = BLOCK_SIZE as usize / 8; // 512

fn read_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}

fn read_u64(buf: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(buf[off..off + 8].try_into().unwrap())
}

fn write_u32(buf: &mut [u8], off: usize, val: u32) {
    buf[off..off + 4].copy_from_slice(&val.to_le_bytes());
}

fn write_u64(buf: &mut [u8], off: usize, val: u64) {
    buf[off..off + 8].copy_from_slice(&val.to_le_bytes());
}

/// On-disk superblock stored in block 0.
struct Superblock {
    block_count: u64,
    bitmap_start: u64,
    bitmap_blocks: u64,
    entry_table_start: u64,
    entry_table_blocks: u64,
    entry_count: u64,
    free_blocks: u64,
}

impl Superblock {
    fn parse(buf: &[u8]) -> Option<Self> {
        if buf[0..4] != MAGIC { return None; }
        if read_u32(buf, 4) != VERSION { return None; }
        Some(Self {
            block_count: read_u64(buf, 8),
            bitmap_start: read_u64(buf, 20),
            bitmap_blocks: read_u64(buf, 28),
            entry_table_start: read_u64(buf, 36),
            entry_table_blocks: read_u64(buf, 44),
            entry_count: read_u64(buf, 52),
            free_blocks: read_u64(buf, 60),
        })
    }

    fn write_to(&self, buf: &mut [u8]) {
        buf[..4096].fill(0);
        buf[0..4].copy_from_slice(&MAGIC);
        write_u32(buf, 4, VERSION);
        write_u64(buf, 8, self.block_count);
        write_u32(buf, 16, BLOCK_SIZE);
        write_u64(buf, 20, self.bitmap_start);
        write_u64(buf, 28, self.bitmap_blocks);
        write_u64(buf, 36, self.entry_table_start);
        write_u64(buf, 44, self.entry_table_blocks);
        write_u64(buf, 52, self.entry_count);
        write_u64(buf, 60, self.free_blocks);
    }
}

/// Block-based filesystem with bitmap allocation and flat entry table.
pub struct ToyFs {
    sb: Superblock,
    next_free_block: u64,
    next_free_entry: u64,
    /// One past the highest known used entry index (for limiting scans).
    entry_watermark: u64,
}

impl ToyFs {
    /// Format a new filesystem on the device.
    pub fn format(cache: &mut PageCache, dev: &mut dyn BlockDevice) -> Self {
        let block_count = dev.block_count();
        let bitmap_start = 1u64;
        let bitmap_blocks = (block_count + BLOCK_SIZE as u64 * 8 - 1) / (BLOCK_SIZE as u64 * 8);
        let entry_table_start = bitmap_start + bitmap_blocks;
        let entry_count = cmp::min(block_count / 16, 65536) as u64;
        let entry_table_blocks = (entry_count * ENTRY_SIZE as u64 + BLOCK_SIZE as u64 - 1)
            / BLOCK_SIZE as u64;
        let metadata_blocks = 1 + bitmap_blocks + entry_table_blocks;
        let free_blocks = block_count - metadata_blocks;

        let sb = Superblock {
            block_count, bitmap_start, bitmap_blocks,
            entry_table_start, entry_table_blocks, entry_count, free_blocks,
        };

        // Write superblock
        let page = cache.write_new(dev, 0);
        sb.write_to(page);

        // Zero bitmap
        for i in 0..bitmap_blocks {
            let page = cache.write_new(dev, bitmap_start + i);
            page.fill(0);
        }

        // Mark metadata blocks as used in bitmap
        let fs = Self { sb, next_free_block: metadata_blocks, next_free_entry: 0, entry_watermark: 0 };
        for b in 0..metadata_blocks {
            fs.bitmap_set(cache, dev, b, true);
        }

        // Zero entry table
        for i in 0..entry_table_blocks {
            let page = cache.write_new(dev, entry_table_start + i);
            page.fill(0);
        }

        // Update free_blocks after marking metadata
        // (we already accounted for metadata_blocks above, bitmap_set doesn't track)
        cache.sync(dev);
        crate::log!("ToyFs: formatted {} blocks ({} MB), {} entries",
            block_count, block_count * 4096 / (1024 * 1024), entry_count);
        fs
    }

    /// Mount an existing filesystem.
    pub fn mount(cache: &mut PageCache, dev: &mut dyn BlockDevice) -> Option<Self> {
        let page = cache.read(dev, 0);
        let sb = Superblock::parse(page)?;
        let data_start = sb.entry_table_start + sb.entry_table_blocks;

        // Scan entry table to find watermark and first free entry
        let mut watermark = 0u64;
        let mut first_free = sb.entry_count;
        for idx in 0..sb.entry_count {
            let block = sb.entry_table_start + idx / ENTRIES_PER_BLOCK as u64;
            let offset = (idx % ENTRIES_PER_BLOCK as u64) as usize * ENTRY_SIZE;
            let page = cache.read(dev, block);
            if page[offset] != 0 {
                watermark = idx + 1;
            } else if first_free == sb.entry_count {
                first_free = idx;
            }
        }

        crate::log!("ToyFs: mounted {} blocks, {} entries ({} used), {} free blocks",
            sb.block_count, sb.entry_count, watermark, sb.free_blocks);
        Some(Self { sb, next_free_block: data_start, next_free_entry: first_free, entry_watermark: watermark })
    }

    // -- Bitmap operations --

    fn bitmap_set(&self, cache: &mut PageCache, dev: &mut dyn BlockDevice, block: u64, used: bool) {
        let byte_idx = block / 8;
        let bit_idx = block % 8;
        let bitmap_block = self.sb.bitmap_start + byte_idx / BLOCK_SIZE as u64;
        let byte_off = (byte_idx % BLOCK_SIZE as u64) as usize;
        let page = cache.write(dev, bitmap_block);
        if used {
            page[byte_off] |= 1 << bit_idx;
        } else {
            page[byte_off] &= !(1 << bit_idx);
        }
    }

    /// Allocate a free block, returns block number.
    fn alloc_block(&mut self, cache: &mut PageCache, dev: &mut dyn BlockDevice) -> Result<u64, &'static str> {
        let start = self.next_free_block;
        let end = self.sb.block_count;

        // Scan bitmap page by page, byte by byte, from hint position
        let start_byte = start / 8;
        let end_byte = (end + 7) / 8;
        let mut byte_pos = start_byte;

        while byte_pos < end_byte {
            let bitmap_block = self.sb.bitmap_start + byte_pos / BLOCK_SIZE as u64;
            let page_start = (byte_pos % BLOCK_SIZE as u64) as usize;
            let page = cache.read(dev, bitmap_block);

            for off in page_start..BLOCK_SIZE as usize {
                if page[off] == 0xFF { continue; }
                // Found a byte with a free bit
                let base = (bitmap_block - self.sb.bitmap_start) * BLOCK_SIZE as u64 * 8
                    + off as u64 * 8;
                for bit in 0..8u64 {
                    let block_num = base + bit;
                    if block_num < start || block_num >= end { continue; }
                    if (page[off] >> bit) & 1 == 0 {
                        self.bitmap_set(cache, dev, block_num, true);
                        self.sb.free_blocks -= 1;
                        self.next_free_block = block_num + 1;
                        return Ok(block_num);
                    }
                }
            }
            // Advance to start of next bitmap page
            byte_pos = (bitmap_block - self.sb.bitmap_start + 1) * BLOCK_SIZE as u64;
        }
        Err("disk full")
    }

    /// Free a block.
    fn free_block(&mut self, cache: &mut PageCache, dev: &mut dyn BlockDevice, block: u64) {
        if block == 0 { return; }
        self.bitmap_set(cache, dev, block, false);
        self.sb.free_blocks += 1;
        if block < self.next_free_block {
            self.next_free_block = block;
        }
    }

    // -- Entry table operations --

    fn read_entry(&self, cache: &mut PageCache, dev: &mut dyn BlockDevice, idx: u64) -> [u8; ENTRY_SIZE] {
        let block = self.sb.entry_table_start + idx / ENTRIES_PER_BLOCK as u64;
        let offset = (idx % ENTRIES_PER_BLOCK as u64) as usize * ENTRY_SIZE;
        let page = cache.read(dev, block);
        let mut entry = [0u8; ENTRY_SIZE];
        entry.copy_from_slice(&page[offset..offset + ENTRY_SIZE]);
        entry
    }

    fn write_entry(&self, cache: &mut PageCache, dev: &mut dyn BlockDevice, idx: u64, entry: &[u8; ENTRY_SIZE]) {
        let block = self.sb.entry_table_start + idx / ENTRIES_PER_BLOCK as u64;
        let offset = (idx % ENTRIES_PER_BLOCK as u64) as usize * ENTRY_SIZE;
        let page = cache.write(dev, block);
        page[offset..offset + ENTRY_SIZE].copy_from_slice(entry);
    }

    fn entry_name(entry: &[u8; ENTRY_SIZE]) -> &str {
        let name_len = read_u32(entry, 4) as usize;
        let name_bytes = &entry[104..104 + name_len.min(NAME_MAX)];
        core::str::from_utf8(name_bytes).unwrap_or("")
    }

    fn find_entry(&self, cache: &mut PageCache, dev: &mut dyn BlockDevice, name: &str) -> Option<u64> {
        for idx in 0..self.entry_watermark {
            let entry = self.read_entry(cache, dev, idx);
            if entry[0] != 0 && Self::entry_name(&entry) == name {
                return Some(idx);
            }
        }
        None
    }

    fn find_free_entry(&mut self, cache: &mut PageCache, dev: &mut dyn BlockDevice) -> Result<u64, &'static str> {
        for idx in self.next_free_entry..self.sb.entry_count {
            let entry = self.read_entry(cache, dev, idx);
            if entry[0] == 0 {
                self.next_free_entry = idx + 1;
                return Ok(idx);
            }
        }
        Err("entry table full")
    }

    // -- Data block operations --

    /// Get the block number for a given block index within a file.
    fn get_data_block(&self, cache: &mut PageCache, dev: &mut dyn BlockDevice,
                      entry: &[u8; ENTRY_SIZE], block_idx: usize) -> u64 {
        if block_idx < DIRECT_BLOCKS {
            read_u64(entry, 24 + block_idx * 8)
        } else if block_idx < DIRECT_BLOCKS + PTRS_PER_INDIRECT {
            let indirect = read_u64(entry, 88);
            if indirect == 0 { return 0; }
            let page = cache.read(dev, indirect);
            read_u64(page, (block_idx - DIRECT_BLOCKS) * 8)
        } else {
            // Double indirect
            let dbl = read_u64(entry, 96);
            if dbl == 0 { return 0; }
            let idx = block_idx - DIRECT_BLOCKS - PTRS_PER_INDIRECT;
            let l1_idx = idx / PTRS_PER_INDIRECT;
            let l2_idx = idx % PTRS_PER_INDIRECT;
            let l1_page = cache.read(dev, dbl);
            let l1_block = read_u64(l1_page, l1_idx * 8);
            if l1_block == 0 { return 0; }
            let l2_page = cache.read(dev, l1_block);
            read_u64(l2_page, l2_idx * 8)
        }
    }

    /// Set a data block pointer in an entry.
    fn set_data_block(&mut self, cache: &mut PageCache, dev: &mut dyn BlockDevice,
                      entry: &mut [u8; ENTRY_SIZE], block_idx: usize, block_num: u64) {
        if block_idx < DIRECT_BLOCKS {
            write_u64(entry, 24 + block_idx * 8, block_num);
        } else if block_idx < DIRECT_BLOCKS + PTRS_PER_INDIRECT {
            let mut indirect = read_u64(entry, 88);
            if indirect == 0 {
                indirect = self.alloc_block_for_indirect(cache, dev);
                write_u64(entry, 88, indirect);
                let page = cache.write_new(dev, indirect);
                page.fill(0);
            }
            let ptr_idx = block_idx - DIRECT_BLOCKS;
            let page = cache.write(dev, indirect);
            write_u64(page, ptr_idx * 8, block_num);
        } else {
            // Double indirect
            let mut dbl = read_u64(entry, 96);
            if dbl == 0 {
                dbl = self.alloc_block_for_indirect(cache, dev);
                write_u64(entry, 96, dbl);
                let page = cache.write_new(dev, dbl);
                page.fill(0);
            }
            let idx = block_idx - DIRECT_BLOCKS - PTRS_PER_INDIRECT;
            let l1_idx = idx / PTRS_PER_INDIRECT;
            let l2_idx = idx % PTRS_PER_INDIRECT;
            // Read/allocate L1 block
            let l1_page = cache.read(dev, dbl);
            let mut l1_block = read_u64(l1_page, l1_idx * 8);
            if l1_block == 0 {
                l1_block = self.alloc_block_for_indirect(cache, dev);
                let page = cache.write(dev, dbl);
                write_u64(page, l1_idx * 8, l1_block);
                let page = cache.write_new(dev, l1_block);
                page.fill(0);
            }
            let l2_page = cache.write(dev, l1_block);
            write_u64(l2_page, l2_idx * 8, block_num);
        }
    }

    /// Allocate a block for indirect pointer storage.
    /// Panics if disk is full — indirect blocks are critical metadata.
    fn alloc_block_for_indirect(&mut self, cache: &mut PageCache, dev: &mut dyn BlockDevice) -> u64 {
        self.alloc_block(cache, dev).expect("disk full: cannot allocate indirect block")
    }

    /// Free all data blocks referenced by an entry.
    fn free_data_blocks(&mut self, cache: &mut PageCache, dev: &mut dyn BlockDevice, entry: &[u8; ENTRY_SIZE]) {
        let size = read_u64(entry, 8) as usize;
        let block_count = (size + 4095) / 4096;

        // Free direct blocks
        for i in 0..cmp::min(block_count, DIRECT_BLOCKS) {
            let b = read_u64(entry, 24 + i * 8);
            if b != 0 { self.free_block(cache, dev, b); }
        }

        // Free indirect blocks
        let indirect = read_u64(entry, 88);
        if indirect != 0 && block_count > DIRECT_BLOCKS {
            let page = cache.read(dev, indirect);
            let indirect_data: Vec<u8> = page.to_vec();
            let indirect_count = (block_count - DIRECT_BLOCKS).min(PTRS_PER_INDIRECT);
            for i in 0..indirect_count {
                let b = read_u64(&indirect_data, i * 8);
                if b != 0 { self.free_block(cache, dev, b); }
            }
            self.free_block(cache, dev, indirect);
        }

        // Free double-indirect blocks
        let dbl = read_u64(entry, 96);
        if dbl != 0 && block_count > DIRECT_BLOCKS + PTRS_PER_INDIRECT {
            let remaining = block_count - DIRECT_BLOCKS - PTRS_PER_INDIRECT;
            let l1_count = (remaining + PTRS_PER_INDIRECT - 1) / PTRS_PER_INDIRECT;
            let dbl_page = cache.read(dev, dbl);
            let dbl_data: Vec<u8> = dbl_page.to_vec();
            for i in 0..l1_count {
                let l1_block = read_u64(&dbl_data, i * 8);
                if l1_block == 0 { continue; }
                let l1_page = cache.read(dev, l1_block);
                let l1_data: Vec<u8> = l1_page.to_vec();
                let ptrs = if i == l1_count - 1 {
                    remaining - i * PTRS_PER_INDIRECT
                } else {
                    PTRS_PER_INDIRECT
                };
                for j in 0..ptrs {
                    let b = read_u64(&l1_data, j * 8);
                    if b != 0 { self.free_block(cache, dev, b); }
                }
                self.free_block(cache, dev, l1_block);
            }
            self.free_block(cache, dev, dbl);
        }
    }

    // -- High-level operations --

    pub fn create_file(&mut self, cache: &mut PageCache, dev: &mut dyn BlockDevice,
                       name: &str, data: &[u8], mtime: u64, entry_type: u8) -> Result<(), &'static str> {
        assert!(name.len() <= NAME_MAX, "filename too long: {}", name);

        // Delete existing entry with same name
        if let Some(idx) = self.find_entry(cache, dev, name) {
            let entry = self.read_entry(cache, dev, idx);
            self.free_data_blocks(cache, dev, &entry);
            self.write_entry(cache, dev, idx, &[0u8; ENTRY_SIZE]);
            if idx < self.next_free_entry { self.next_free_entry = idx; }
        }

        let idx = self.find_free_entry(cache, dev)?;
        let block_count = (data.len() + 4095) / 4096;

        // Build entry
        let mut entry = [0u8; ENTRY_SIZE];
        entry[0] = entry_type;
        write_u32(&mut entry, 4, name.len() as u32);
        write_u64(&mut entry, 8, data.len() as u64);
        write_u64(&mut entry, 16, mtime);
        entry[104..104 + name.len()].copy_from_slice(name.as_bytes());

        // Allocate and write data blocks
        for i in 0..block_count {
            let b = self.alloc_block(cache, dev)?;
            self.set_data_block(cache, dev, &mut entry, i, b);

            let start = i * 4096;
            let end = cmp::min(start + 4096, data.len());
            let page = cache.write_new(dev, b);
            page[..end - start].copy_from_slice(&data[start..end]);
        }

        self.write_entry(cache, dev, idx, &entry);
        if idx + 1 > self.entry_watermark {
            self.entry_watermark = idx + 1;
        }
        Ok(())
    }

    pub fn read_file(&self, cache: &mut PageCache, dev: &mut dyn BlockDevice,
                     name: &str) -> Result<Vec<u8>, &'static str> {
        let idx = self.find_entry(cache, dev, name).ok_or("not found")?;
        let entry = self.read_entry(cache, dev, idx);
        let size = read_u64(&entry, 8) as usize;
        let block_count = (size + 4095) / 4096;

        // Collect all block numbers for prefetching
        let mut blocks = Vec::with_capacity(block_count);
        for i in 0..block_count {
            let b = self.get_data_block(cache, dev, &entry, i);
            if b == 0 { break; }
            blocks.push(b);
        }

        // Prefetch all blocks with batched I/O (up to 32 blocks per device call)
        cache.prefetch(dev, &blocks);

        let mut result = vec![0u8; size];
        for (i, &b) in blocks.iter().enumerate() {
            let page = cache.read(dev, b);
            let start = i * 4096;
            let end = cmp::min(start + 4096, size);
            result[start..end].copy_from_slice(&page[..end - start]);
        }
        Ok(result)
    }

    pub fn delete(&mut self, cache: &mut PageCache, dev: &mut dyn BlockDevice,
                  name: &str) -> bool {
        let Some(idx) = self.find_entry(cache, dev, name) else { return false };
        let entry = self.read_entry(cache, dev, idx);
        self.free_data_blocks(cache, dev, &entry);
        self.write_entry(cache, dev, idx, &[0u8; ENTRY_SIZE]);
        if idx < self.next_free_entry { self.next_free_entry = idx; }
        true
    }

    pub fn delete_prefix(&mut self, cache: &mut PageCache, dev: &mut dyn BlockDevice, prefix: &str) {
        for idx in 0..self.entry_watermark {
            let entry = self.read_entry(cache, dev, idx);
            if entry[0] != 0 && Self::entry_name(&entry).starts_with(prefix) {
                self.free_data_blocks(cache, dev, &entry);
                self.write_entry(cache, dev, idx, &[0u8; ENTRY_SIZE]);
                if idx < self.next_free_entry { self.next_free_entry = idx; }
            }
        }
    }

    pub fn list(&self, cache: &mut PageCache, dev: &mut dyn BlockDevice) -> Vec<(String, u64)> {
        let mut result = Vec::new();
        for idx in 0..self.entry_watermark {
            let entry = self.read_entry(cache, dev, idx);
            if entry[0] != 0 {
                let name = String::from(Self::entry_name(&entry));
                let size = read_u64(&entry, 8);
                result.push((name, size));
            }
        }
        result
    }

    pub fn read_link(&self, cache: &mut PageCache, dev: &mut dyn BlockDevice,
                     name: &str) -> Option<String> {
        let idx = self.find_entry(cache, dev, name)?;
        let entry = self.read_entry(cache, dev, idx);
        if entry[0] != 2 { return None; }
        let data = self.read_file(cache, dev, name).ok()?;
        String::from_utf8(data).ok()
    }

    pub fn file_mtime(&self, cache: &mut PageCache, dev: &mut dyn BlockDevice,
                      name: &str) -> u64 {
        let Some(idx) = self.find_entry(cache, dev, name) else { return 0 };
        let entry = self.read_entry(cache, dev, idx);
        read_u64(&entry, 16)
    }

    pub fn sync(&mut self, cache: &mut PageCache, dev: &mut dyn BlockDevice) {
        // Write updated superblock
        let page = cache.write(dev, 0);
        self.sb.write_to(page);
        cache.sync(dev);
    }
}

// -- VFS integration --

/// Adapter that bundles ToyFs + BlockDevice and implements the VFS FileSystem trait.
/// Acquires the page cache lock internally for each operation.
pub struct ToyFsAdapter {
    fs: ToyFs,
    dev: crate::drivers::nvme::NvmeBlockDevice,
}

impl ToyFsAdapter {
    pub fn new(fs: ToyFs, dev: crate::drivers::nvme::NvmeBlockDevice) -> Self {
        Self { fs, dev }
    }
}

impl FileSystem for ToyFsAdapter {
    fn list(&mut self) -> Vec<(String, u64)> {
        let mut cache = crate::page_cache::lock();
        self.fs.list(&mut *cache, &mut self.dev)
    }

    fn read_file(&mut self, name: &str) -> Result<Cow<'static, [u8]>, &'static str> {
        let mut cache = crate::page_cache::lock();
        self.fs.read_file(&mut *cache, &mut self.dev, name).map(Cow::Owned)
    }

    fn read_link(&mut self, name: &str) -> Option<String> {
        let mut cache = crate::page_cache::lock();
        self.fs.read_link(&mut *cache, &mut self.dev, name)
    }

    fn file_mtime(&mut self, name: &str) -> u64 {
        let mut cache = crate::page_cache::lock();
        self.fs.file_mtime(&mut *cache, &mut self.dev, name)
    }

    fn create(&mut self, name: &str, data: &[u8], mtime: u64) -> Result<(), &'static str> {
        let mut cache = crate::page_cache::lock();
        self.fs.create_file(&mut *cache, &mut self.dev, name, data, mtime, 1)
    }

    fn delete(&mut self, name: &str) -> bool {
        let mut cache = crate::page_cache::lock();
        self.fs.delete(&mut *cache, &mut self.dev, name)
    }

    fn delete_prefix(&mut self, prefix: &str) {
        let mut cache = crate::page_cache::lock();
        self.fs.delete_prefix(&mut *cache, &mut self.dev, prefix)
    }

    fn create_symlink(&mut self, name: &str, target: &str) -> Result<(), &'static str> {
        let mut cache = crate::page_cache::lock();
        self.fs.create_file(&mut *cache, &mut self.dev, name, target.as_bytes(), 0, 2)
    }

    fn sync(&mut self) {
        let mut cache = crate::page_cache::lock();
        self.fs.sync(&mut *cache, &mut self.dev)
    }
}
