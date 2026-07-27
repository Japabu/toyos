use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::marker::PhantomData;

use crate::alloc_bitmap::BitmapAllocator;
use crate::block_io::{BlockBuf, BlockNum, BlockIO, BLOCK_SIZE};
use crate::btree::{self, Entry, Key, KeyType, Node};
use crate::superblock::Superblock;

/// Extent: a contiguous run of blocks on disk.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Extent {
    pub start_block: u64,
    pub block_count: u32,
    pub _reserved: u32,
}

const EXTENT_SIZE: usize = 16;

/// Filesystem error type with rich context.
#[derive(Debug)]
pub enum FsError {
    BadMagic { expected: [u8; 4], got: [u8; 4] },
    UnsupportedVersion(u32),
    ChecksumMismatch { block: BlockNum, stored: u32, computed: u32 },
    CorruptedKey(u16),
    CorruptedNode(BlockNum),
    NotFound,
    NoSpace { requested: u32, available: u64 },
    NameTooLong { len: usize, max: usize },
}

pub struct ReadOnly;
pub struct ReadWrite;

/// A formatted but not yet mounted filesystem. Used for building images (mkfs).
pub struct Formatted<IO: BlockIO> {
    io: IO,
    sb: Superblock,
    alloc: BitmapAllocator,
}

/// A mounted filesystem. Mode is ReadOnly or ReadWrite.
pub struct Mounted<IO: BlockIO, Mode = ReadWrite> {
    io: IO,
    sb: Superblock,
    alloc: BitmapAllocator,
    _mode: PhantomData<Mode>,
}

// --- Hash ---

fn siphash_2_4(data: &[u8], key: [u8; 8]) -> u64 {
    // Simplified SipHash-2-4
    let k = u64::from_le_bytes(key);
    let mut v0 = 0x736f6d6570736575u64 ^ k;
    let mut v1 = 0x646f72616e646f6du64 ^ k;
    let mut v2 = 0x6c7967656e657261u64 ^ k;
    let mut v3 = 0x7465646279746573u64 ^ k;

    let len = data.len();
    let blocks = len / 8;

    for i in 0..blocks {
        let mut word = [0u8; 8];
        word.copy_from_slice(&data[i * 8..i * 8 + 8]);
        let m = u64::from_le_bytes(word);
        v3 ^= m;
        for _ in 0..2 {
            sip_round(&mut v0, &mut v1, &mut v2, &mut v3);
        }
        v0 ^= m;
    }

    let mut last = (len as u64) << 56;
    let remainder = &data[blocks * 8..];
    for (i, &byte) in remainder.iter().enumerate() {
        last |= (byte as u64) << (i * 8);
    }

    v3 ^= last;
    for _ in 0..2 {
        sip_round(&mut v0, &mut v1, &mut v2, &mut v3);
    }
    v0 ^= last;

    v2 ^= 0xff;
    for _ in 0..4 {
        sip_round(&mut v0, &mut v1, &mut v2, &mut v3);
    }

    v0 ^ v1 ^ v2 ^ v3
}

fn sip_round(v0: &mut u64, v1: &mut u64, v2: &mut u64, v3: &mut u64) {
    *v0 = v0.wrapping_add(*v1);
    *v1 = v1.rotate_left(13);
    *v1 ^= *v0;
    *v0 = v0.rotate_left(32);
    *v2 = v2.wrapping_add(*v3);
    *v3 = v3.rotate_left(16);
    *v3 ^= *v2;
    *v0 = v0.wrapping_add(*v3);
    *v3 = v3.rotate_left(21);
    *v3 ^= *v0;
    *v2 = v2.wrapping_add(*v1);
    *v1 = v1.rotate_left(17);
    *v1 ^= *v2;
    *v2 = v2.rotate_left(32);
}

fn hash_name(seed: &[u8; 16], name: &str) -> (u64, u64) {
    let mut seed1 = [0u8; 8];
    let mut seed2 = [0u8; 8];
    seed1.copy_from_slice(&seed[0..8]);
    seed2.copy_from_slice(&seed[8..16]);
    (
        siphash_2_4(name.as_bytes(), seed1),
        siphash_2_4(name.as_bytes(), seed2),
    )
}

fn make_key(seed: &[u8; 16], name: &str, key_type: KeyType) -> Key {
    let (h, hi) = hash_name(seed, name);
    Key {
        name_hash: h,
        name_hash_hi: hi,
        key_type,
    }
}

// --- Leaf value encoding/decoding ---

const MAX_NAME_LEN: usize = 512;

/// Encode a file/symlink leaf value.
fn encode_leaf_value(
    entry_type: u8,
    name: &str,
    size: u64,
    mtime: u64,
    extents: &[Extent],
) -> Vec<u8> {
    let name_bytes = name.as_bytes();
    let name_len = name_bytes.len();
    // 1 (entry_type) + 2 (name_len) + 8 (size) + 8 (mtime) + name + extents
    let extent_bytes = extents.len() * EXTENT_SIZE;
    let total = 1 + 2 + 8 + 8 + name_len + extent_bytes;
    let mut val = vec![0u8; total];

    val[0] = entry_type;
    val[1..3].copy_from_slice(&(name_len as u16).to_le_bytes());
    val[3..11].copy_from_slice(&size.to_le_bytes());
    val[11..19].copy_from_slice(&mtime.to_le_bytes());
    val[19..19 + name_len].copy_from_slice(name_bytes);

    let mut off = 19 + name_len;
    for ext in extents {
        val[off..off + 8].copy_from_slice(&ext.start_block.to_le_bytes());
        val[off + 8..off + 12].copy_from_slice(&ext.block_count.to_le_bytes());
        val[off + 12..off + 16].copy_from_slice(&ext._reserved.to_le_bytes());
        off += EXTENT_SIZE;
    }

    val
}

/// Decoded leaf value with owned strings.
pub enum LeafValue {
    File {
        name: String,
        size: u64,
        mtime: u64,
        extents: Vec<Extent>,
    },
    Symlink {
        name: String,
        size: u64,
        mtime: u64,
        extents: Vec<Extent>,
    },
}

impl LeafValue {
    pub fn name(&self) -> &str {
        match self {
            LeafValue::File { name, .. } => name,
            LeafValue::Symlink { name, .. } => name,
        }
    }

    pub fn size(&self) -> u64 {
        match self {
            LeafValue::File { size, .. } => *size,
            LeafValue::Symlink { size, .. } => *size,
        }
    }

    pub fn mtime(&self) -> u64 {
        match self {
            LeafValue::File { mtime, .. } => *mtime,
            LeafValue::Symlink { mtime, .. } => *mtime,
        }
    }

    pub fn extents(&self) -> &[Extent] {
        match self {
            LeafValue::File { extents, .. } => extents,
            LeafValue::Symlink { extents, .. } => extents,
        }
    }
}

fn decode_leaf_value(value: &[u8]) -> Result<LeafValue, FsError> {
    if value.len() < 19 {
        return Err(FsError::CorruptedKey(0));
    }

    let entry_type = value[0];
    let name_len = u16::from_le_bytes([value[1], value[2]]) as usize;
    let size = u64::from_le_bytes(value[3..11].try_into().unwrap());
    let mtime = u64::from_le_bytes(value[11..19].try_into().unwrap());

    if 19 + name_len > value.len() {
        return Err(FsError::CorruptedKey(0));
    }

    let name_str = core::str::from_utf8(&value[19..19 + name_len])
        .map_err(|_| FsError::CorruptedKey(0))?;
    let name = String::from(name_str);

    let extent_data = &value[19 + name_len..];
    let extent_count = extent_data.len() / EXTENT_SIZE;
    let mut extents = Vec::with_capacity(extent_count);
    for i in 0..extent_count {
        let off = i * EXTENT_SIZE;
        extents.push(Extent {
            start_block: u64::from_le_bytes(extent_data[off..off + 8].try_into().unwrap()),
            block_count: u32::from_le_bytes(extent_data[off + 8..off + 12].try_into().unwrap()),
            _reserved: 0,
        });
    }

    match entry_type {
        1 => Ok(LeafValue::File { name, size, mtime, extents }),
        2 => Ok(LeafValue::Symlink { name, size, mtime, extents }),
        _ => Err(FsError::CorruptedKey(entry_type as u16)),
    }
}

/// Read file data from a list of extents.
fn read_extents(io: &dyn BlockIO, extents: &[Extent], size: u64) -> Vec<u8> {
    let mut data = vec![0u8; size as usize];
    let mut offset = 0usize;
    let mut buf = BlockBuf::zeroed();

    for ext in extents {
        for i in 0..ext.block_count as u64 {
            if offset >= size as usize {
                break;
            }
            io.read_block(BlockNum::new(ext.start_block + i), &mut buf);
            let remaining = size as usize - offset;
            let to_copy = remaining.min(BLOCK_SIZE);
            data[offset..offset + to_copy].copy_from_slice(&buf.0[..to_copy]);
            offset += to_copy;
        }
    }

    data
}

// --- Formatted (for mkfs / image building) ---

impl<IO: BlockIO> Formatted<IO> {
    /// Format a new filesystem on the given block device.
    pub fn format(io: IO) -> Self {
        let block_count = io.block_count();

        // Layout: [superblock(1)] [bitmap] [journal(reserved, 0 for now)] [data...] [sb_backup(1)]
        let bitmap_blocks = (block_count + BLOCK_SIZE as u64 * 8 - 1) / (BLOCK_SIZE as u64 * 8);
        let bitmap_start = BlockNum::new(1);
        // Reserve journal space but don't use it in Phase 1
        let journal_start = BlockNum::new(1 + bitmap_blocks);
        let journal_blocks = 0u32; // Phase 2 will set this to 64

        let metadata_blocks = 1 + bitmap_blocks + journal_blocks as u64;

        // Create empty root leaf node
        let root_block_num = metadata_blocks; // first data block is the root node
        let total_metadata = metadata_blocks + 1; // +1 for root node

        let alloc = BitmapAllocator::format(
            &io,
            bitmap_start,
            bitmap_blocks,
            block_count,
            total_metadata,
        );

        // Write empty root leaf
        let root = Node {
            level: 0,
            entries: Vec::new(),
        };
        root.write(&io, BlockNum::new(root_block_num));

        // Generate random-ish hash seed from block count (deterministic for reproducible builds)
        let mut hash_seed = [0u8; 16];
        let seed_val = block_count.wrapping_mul(0x517cc1b727220a95);
        hash_seed[0..8].copy_from_slice(&seed_val.to_le_bytes());
        hash_seed[8..16].copy_from_slice(&seed_val.wrapping_mul(0x6c62272e07bb0142).to_le_bytes());

        let sb = Superblock {
            block_count,
            root_node: BlockNum::new(root_block_num),
            root_level: 0,
            next_alloc: total_metadata,
            free_blocks: alloc.free_blocks,
            bitmap_start,
            bitmap_blocks,
            journal_start,
            journal_blocks,
            journal_head: 0,
            flags: 0, // not clean until sync
            hash_seed,
        };

        sb.write(&io);

        Self { io, sb, alloc }
    }

    /// Create a file on the formatted filesystem (used during mkfs).
    pub fn create(&mut self, name: &str, data: &[u8], mtime: u64) -> Result<(), FsError> {
        if name.is_empty() || name.len() > MAX_NAME_LEN {
            return Err(FsError::NameTooLong { len: name.len(), max: MAX_NAME_LEN });
        }

        let extents = self.write_data(data)?;
        let value = encode_leaf_value(1, name, data.len() as u64, mtime, &extents);
        let key = make_key(&self.sb.hash_seed, name, KeyType::File);
        let entry = Entry { key, value };

        let (new_root, new_level) =
            btree::insert(&self.io, &mut self.alloc, self.sb.root_node, self.sb.root_level, entry)?;
        self.sb.root_node = new_root;
        self.sb.root_level = new_level;

        Ok(())
    }

    /// Create a symlink on the formatted filesystem.
    pub fn create_symlink(&mut self, name: &str, target: &str, mtime: u64) -> Result<(), FsError> {
        if name.is_empty() || name.len() > MAX_NAME_LEN {
            return Err(FsError::NameTooLong { len: name.len(), max: MAX_NAME_LEN });
        }

        let target_bytes = target.as_bytes();
        let extents = self.write_data(target_bytes)?;
        let value = encode_leaf_value(2, name, target_bytes.len() as u64, mtime, &extents);
        let key = make_key(&self.sb.hash_seed, name, KeyType::Symlink);
        let entry = Entry { key, value };

        let (new_root, new_level) =
            btree::insert(&self.io, &mut self.alloc, self.sb.root_node, self.sb.root_level, entry)?;
        self.sb.root_node = new_root;
        self.sb.root_level = new_level;

        Ok(())
    }

    /// Allocate blocks and write data, returning extent list.
    fn write_data(&mut self, data: &[u8]) -> Result<Vec<Extent>, FsError> {
        if data.is_empty() {
            return Ok(Vec::new());
        }

        let blocks_needed = ((data.len() + BLOCK_SIZE - 1) / BLOCK_SIZE) as u32;
        let mut extents = Vec::new();
        let mut remaining = blocks_needed;
        let mut data_offset = 0usize;

        while remaining > 0 {
            let (start, count) = self.alloc.alloc_contiguous(&self.io, remaining)?;
            extents.push(Extent {
                start_block: start.raw(),
                block_count: count,
                _reserved: 0,
            });

            // Write data blocks
            let mut buf = BlockBuf::zeroed();
            for i in 0..count as u64 {
                buf.0.fill(0);
                let chunk_start = data_offset;
                let chunk_end = (data_offset + BLOCK_SIZE).min(data.len());
                if chunk_start < data.len() {
                    let len = chunk_end - chunk_start;
                    buf.0[..len].copy_from_slice(&data[chunk_start..chunk_end]);
                }
                self.io.write_block(BlockNum::new(start.raw() + i), &buf);
                data_offset += BLOCK_SIZE;
            }

            remaining -= count;
        }

        Ok(extents)
    }

    /// Finalize the filesystem: write superblock with clean flag.
    pub fn sync(&mut self) {
        self.sb.free_blocks = self.alloc.free_blocks;
        self.sb.next_alloc = self.alloc.next_alloc;
        self.sb.set_clean(true);
        self.sb.write(&self.io);
        self.io.sync();
    }

    /// Mount this formatted filesystem for read-write access.
    pub fn mount(self) -> Mounted<IO, ReadWrite> {
        Mounted {
            io: self.io,
            sb: self.sb,
            alloc: self.alloc,
            _mode: PhantomData,
        }
    }

    /// Mount this formatted filesystem for read-only access.
    pub fn mount_readonly(self) -> Mounted<IO, ReadOnly> {
        Mounted {
            io: self.io,
            sb: self.sb,
            alloc: self.alloc,
            _mode: PhantomData,
        }
    }

    /// Consume and return the underlying IO (for extracting the image bytes).
    pub fn into_io(mut self) -> IO {
        self.sync();
        self.io
    }
}

// --- Mounted (read operations, available for both ReadOnly and ReadWrite) ---

impl<IO: BlockIO, Mode> Mounted<IO, Mode> {
    /// Open an existing filesystem from disk.
    pub fn open(io: IO) -> Result<Mounted<IO, Mode>, FsError> {
        let sb = Superblock::read(&io)?;
        let alloc = BitmapAllocator {
            bitmap_start: sb.bitmap_start,
            bitmap_blocks: sb.bitmap_blocks,
            total_blocks: sb.block_count,
            free_blocks: sb.free_blocks,
            next_alloc: sb.next_alloc,
        };
        Ok(Mounted {
            io,
            sb,
            alloc,
            _mode: PhantomData,
        })
    }

    /// Find a file entry by name. Tries File key first, then Symlink.
    fn find_by_name(&self, name: &str) -> Result<Option<(Key, Vec<u8>)>, FsError> {
        // Try as File first (most common)
        let key = make_key(&self.sb.hash_seed, name, KeyType::File);
        if let Some(value) = btree::search(&self.io, self.sb.root_node, self.sb.root_level, &key)? {
            let leaf = decode_leaf_value(&value)?;
            if leaf.name() == name {
                return Ok(Some((key, value)));
            }
        }

        // Try as Symlink
        let key = make_key(&self.sb.hash_seed, name, KeyType::Symlink);
        if let Some(value) = btree::search(&self.io, self.sb.root_node, self.sb.root_level, &key)? {
            let leaf = decode_leaf_value(&value)?;
            if leaf.name() == name {
                return Ok(Some((key, value)));
            }
        }

        Ok(None)
    }

    /// Read a file's contents by name.
    pub fn read_file(&self, name: &str) -> Result<Vec<u8>, FsError> {
        let (_, value) = self.find_by_name(name)?.ok_or(FsError::NotFound)?;
        let leaf = decode_leaf_value(&value)?;
        match leaf {
            LeafValue::File { size, extents, .. } | LeafValue::Symlink { size, extents, .. } => {
                Ok(read_extents(&self.io, &extents, size))
            }
        }
    }

    /// Read a symlink's target by name, or None if not a symlink.
    pub fn read_link(&self, name: &str) -> Option<String> {
        let (_, value) = self.find_by_name(name).ok()??;
        let leaf = decode_leaf_value(&value).ok()?;
        match leaf {
            LeafValue::Symlink { size, extents, .. } => {
                let data = read_extents(&self.io, &extents, size);
                String::from_utf8(data).ok()
            }
            _ => None,
        }
    }

    /// Get modification time of a file.
    pub fn file_mtime(&self, name: &str) -> u64 {
        self.find_by_name(name)
            .ok()
            .flatten()
            .and_then(|(_, v)| decode_leaf_value(&v).ok())
            .map(|leaf| leaf.mtime())
            .unwrap_or(0)
    }

    /// List all files. Returns (name, size) pairs.
    pub fn list(&self) -> Result<Vec<(String, u64)>, FsError> {
        let entries = btree::collect_all(&self.io, self.sb.root_node, self.sb.root_level)?;
        let mut result = Vec::new();
        for entry in &entries {
            if let Ok(leaf) = decode_leaf_value(&entry.value) {
                result.push((String::from(leaf.name()), leaf.size()));
            }
        }
        Ok(result)
    }

    /// Convert back to Formatted state (for testing — insert more files after reading).
    pub fn into_formatted(self) -> Formatted<IO> {
        Formatted {
            io: self.io,
            sb: self.sb,
            alloc: self.alloc,
        }
    }

    /// Return the extents and file size for a file.
    /// Used by the kernel to construct a FileBacking for demand-paged loading.
    pub fn file_extents(&self, name: &str) -> Option<(Vec<Extent>, u64)> {
        let (_, value) = self.find_by_name(name).ok()??;
        let leaf = decode_leaf_value(&value).ok()?;
        Some((leaf.extents().to_vec(), leaf.size()))
    }

    /// Check if a name is a symlink.
    pub fn is_symlink(&self, name: &str) -> bool {
        self.find_by_name(name)
            .ok()
            .flatten()
            .map(|(key, _)| key.key_type == KeyType::Symlink)
            .unwrap_or(false)
    }
    /// Get file size without reading data (metadata only).
    pub fn file_size_meta(&self, name: &str) -> Option<u64> {
        let (_, value) = self.find_by_name(name).ok()??;
        decode_leaf_value(&value).ok().map(|l| l.size())
    }
}

// --- ReadWrite-only operations ---

impl<IO: BlockIO> Mounted<IO, ReadWrite> {
    /// Create a file.
    pub fn create(&mut self, name: &str, data: &[u8], mtime: u64) -> Result<(), FsError> {
        if name.is_empty() || name.len() > MAX_NAME_LEN {
            return Err(FsError::NameTooLong { len: name.len(), max: MAX_NAME_LEN });
        }

        // Delete existing entry with same name (if any) to free its blocks
        self.delete_by_name(name);

        let extents = self.write_data(data)?;
        let value = encode_leaf_value(1, name, data.len() as u64, mtime, &extents);
        let key = make_key(&self.sb.hash_seed, name, KeyType::File);
        let entry = Entry { key, value };

        let (new_root, new_level) =
            btree::insert(&self.io, &mut self.alloc, self.sb.root_node, self.sb.root_level, entry)?;
        self.sb.root_node = new_root;
        self.sb.root_level = new_level;
        Ok(())
    }

    /// Create a symlink.
    pub fn create_symlink(&mut self, name: &str, target: &str) -> Result<(), FsError> {
        if name.is_empty() || name.len() > MAX_NAME_LEN {
            return Err(FsError::NameTooLong { len: name.len(), max: MAX_NAME_LEN });
        }

        self.delete_by_name(name);

        let target_bytes = target.as_bytes();
        let extents = self.write_data(target_bytes)?;
        let value = encode_leaf_value(2, name, target_bytes.len() as u64, 0, &extents);
        let key = make_key(&self.sb.hash_seed, name, KeyType::Symlink);
        let entry = Entry { key, value };

        let (new_root, new_level) =
            btree::insert(&self.io, &mut self.alloc, self.sb.root_node, self.sb.root_level, entry)?;
        self.sb.root_node = new_root;
        self.sb.root_level = new_level;
        Ok(())
    }

    /// Delete a file or symlink by name. Returns true if found and deleted.
    pub fn delete(&mut self, name: &str) -> bool {
        self.delete_by_name(name)
    }

    /// Delete all entries whose name starts with the given prefix.
    pub fn delete_prefix(&mut self, prefix: &str) {
        // Collect entries, find matching ones, free their blocks, remove from tree
        let entries = match btree::collect_all(&self.io, self.sb.root_node, self.sb.root_level) {
            Ok(e) => e,
            Err(_) => return,
        };

        for entry in &entries {
            if let Ok(leaf) = decode_leaf_value(&entry.value) {
                if leaf.name().starts_with(prefix) {
                    // Free data blocks
                    for ext in leaf.extents() {
                        self.alloc.free_range(&self.io, BlockNum::new(ext.start_block), ext.block_count);
                    }
                    // Delete from btree
                    let _ = btree::delete(&self.io, self.sb.root_node, self.sb.root_level, &entry.key);
                }
            }
        }
    }

    /// Sync filesystem state to disk.
    pub fn sync(&mut self) {
        self.sb.free_blocks = self.alloc.free_blocks;
        self.sb.next_alloc = self.alloc.next_alloc;
        self.sb.set_clean(true);
        self.sb.write(&self.io);
        self.io.sync();
    }

    /// Allocate blocks and write data, returning extent list.
    fn write_data(&mut self, data: &[u8]) -> Result<Vec<Extent>, FsError> {
        if data.is_empty() {
            return Ok(Vec::new());
        }

        let blocks_needed = ((data.len() + BLOCK_SIZE - 1) / BLOCK_SIZE) as u32;
        let mut extents = Vec::new();
        let mut remaining = blocks_needed;
        let mut data_offset = 0usize;

        while remaining > 0 {
            let (start, count) = self.alloc.alloc_contiguous(&self.io, remaining)?;
            extents.push(Extent {
                start_block: start.raw(),
                block_count: count,
                _reserved: 0,
            });

            let mut buf = BlockBuf::zeroed();
            for i in 0..count as u64 {
                buf.0.fill(0);
                let chunk_start = data_offset;
                let chunk_end = (data_offset + BLOCK_SIZE).min(data.len());
                if chunk_start < data.len() {
                    let len = chunk_end - chunk_start;
                    buf.0[..len].copy_from_slice(&data[chunk_start..chunk_end]);
                }
                self.io.write_block(BlockNum::new(start.raw() + i), &buf);
                data_offset += BLOCK_SIZE;
            }

            remaining -= count;
        }

        Ok(extents)
    }

    /// Delete a file/symlink by name, freeing its data blocks. Returns true if found.
    fn delete_by_name(&mut self, name: &str) -> bool {
        // Try File key
        let key = make_key(&self.sb.hash_seed, name, KeyType::File);
        if let Ok(Some(value)) = btree::delete(&self.io, self.sb.root_node, self.sb.root_level, &key) {
            if let Ok(leaf) = decode_leaf_value(&value) {
                if leaf.name() == name {
                    for ext in leaf.extents() {
                        self.alloc.free_range(&self.io, BlockNum::new(ext.start_block), ext.block_count);
                    }
                    return true;
                }
            }
        }

        // Try Symlink key
        let key = make_key(&self.sb.hash_seed, name, KeyType::Symlink);
        if let Ok(Some(value)) = btree::delete(&self.io, self.sb.root_node, self.sb.root_level, &key) {
            if let Ok(leaf) = decode_leaf_value(&value) {
                if leaf.name() == name {
                    for ext in leaf.extents() {
                        self.alloc.free_range(&self.io, BlockNum::new(ext.start_block), ext.block_count);
                    }
                    return true;
                }
            }
        }

        false
    }

    /// Rename a file or symlink. Crash-safe ordering: insert new, then delete old.
    pub fn rename(&mut self, old_name: &str, new_name: &str) -> Result<(), FsError> {
        // 1. Find old entry
        let (old_key, old_value) = self.find_by_name(old_name)?
            .ok_or(FsError::NotFound)?;
        let leaf = decode_leaf_value(&old_value)?;

        // 2. INSERT new entry first (crash-safe: duplicate better than loss)
        let entry_type = if old_key.key_type == KeyType::File { 1 } else { 2 };
        let new_value = encode_leaf_value(
            entry_type, new_name, leaf.size(), leaf.mtime(), leaf.extents(),
        );
        let new_key = make_key(&self.sb.hash_seed, new_name, old_key.key_type);
        let (new_root, new_level) = btree::insert(
            &self.io, &mut self.alloc,
            self.sb.root_node, self.sb.root_level,
            Entry { key: new_key, value: new_value },
        )?;
        self.sb.root_node = new_root;
        self.sb.root_level = new_level;

        // 3. DELETE target's old entry if it existed (frees target's blocks)
        self.delete_by_name(new_name);

        // 4. DELETE source's old entry (without freeing blocks — they're in the new entry)
        if let Ok(Some(_)) = btree::delete(&self.io, self.sb.root_node, self.sb.root_level, &old_key) {
            // Blocks are NOT freed — they now belong to the new entry
        }

        Ok(())
    }

    /// Update file metadata (size, mtime, extents) without rewriting data.
    pub fn update_metadata(
        &mut self,
        name: &str,
        new_extents: &[Extent],
        size: u64,
        mtime: u64,
    ) -> Result<(), FsError> {
        // Find and delete old entry
        let (old_key, old_value) = self.find_by_name(name)?
            .ok_or(FsError::NotFound)?;
        let leaf = decode_leaf_value(&old_value)?;
        let entry_type = if old_key.key_type == KeyType::File { 1 } else { 2 };

        // Delete old entry (don't free blocks — we're keeping them)
        btree::delete(&self.io, self.sb.root_node, self.sb.root_level, &old_key)?;

        // If extents changed, free blocks that are no longer needed
        // (For now, just use the new extents as-is)
        let extents = if new_extents.is_empty() { leaf.extents() } else { new_extents };

        // Insert updated entry
        let new_value = encode_leaf_value(entry_type, leaf.name(), size, mtime, extents);
        let (new_root, new_level) = btree::insert(
            &self.io, &mut self.alloc,
            self.sb.root_node, self.sb.root_level,
            Entry { key: old_key, value: new_value },
        )?;
        self.sb.root_node = new_root;
        self.sb.root_level = new_level;
        Ok(())
    }

    /// Resolve a page index to a block number, allocating a new block if needed.
    /// Returns (block_number, extents_were_extended).
    pub fn resolve_or_alloc_block(
        &mut self,
        extents: &mut Vec<Extent>,
        page_idx: u32,
    ) -> Result<u64, FsError> {
        // Check if page_idx is within existing extents
        let mut cursor = 0u32;
        for ext in extents.iter() {
            if page_idx < cursor + ext.block_count {
                return Ok(ext.start_block + (page_idx - cursor) as u64);
            }
            cursor += ext.block_count;
        }

        // Page is beyond existing extents — allocate new blocks to cover it
        let needed = page_idx + 1 - cursor;
        let (start, count) = self.alloc.alloc_contiguous(&self.io, needed)?;
        extents.push(Extent {
            start_block: start.raw(),
            block_count: count,
            _reserved: 0,
        });
        // The requested page_idx is at offset (page_idx - cursor) within the new extent
        Ok(start.raw() + (page_idx - cursor) as u64)
    }
}
