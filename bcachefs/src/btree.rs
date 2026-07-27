use alloc::vec::Vec;
use crate::block_io::{BlockBuf, BlockNum, BlockIO, BLOCK_SIZE};
use crate::crc32c::crc32c;
use crate::alloc_bitmap::BitmapAllocator;
use crate::fs::FsError;

pub const NODE_MAGIC: [u8; 4] = *b"BTND";
const NODE_HEADER_SIZE: usize = 32;
const KEY_HEADER_SIZE: usize = 24;
const CRC_START: usize = 8; // CRC covers bytes [8..4096]
const MAX_PAYLOAD: usize = BLOCK_SIZE - NODE_HEADER_SIZE;

/// On-disk key stored in B+ tree nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    pub name_hash: u64,
    pub name_hash_hi: u64,
    pub key_type: KeyType,
}

impl Key {
    pub const ZERO: Self = Self {
        name_hash: 0,
        name_hash_hi: 0,
        key_type: KeyType::Deleted,
    };
}

impl PartialOrd for Key {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Key {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.name_hash
            .cmp(&other.name_hash)
            .then(self.name_hash_hi.cmp(&other.name_hash_hi))
            .then((self.key_type as u16).cmp(&(other.key_type as u16)))
    }
}

#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum KeyType {
    Deleted = 0,
    File = 1,
    Symlink = 2,
}

impl TryFrom<u16> for KeyType {
    type Error = FsError;
    fn try_from(v: u16) -> Result<Self, FsError> {
        match v {
            0 => Ok(Self::Deleted),
            1 => Ok(Self::File),
            2 => Ok(Self::Symlink),
            _ => Err(FsError::CorruptedKey(v)),
        }
    }
}

/// A key-value entry in a B+ tree node.
#[derive(Debug, Clone)]
pub struct Entry {
    pub key: Key,
    pub value: Vec<u8>,
}

impl Entry {
    /// Total size on disk: key header + value, padded to 8-byte alignment.
    pub fn disk_size(&self) -> usize {
        let raw = KEY_HEADER_SIZE + self.value.len();
        (raw + 7) & !7
    }
}

/// Parsed B+ tree node.
pub struct Node {
    pub level: u16,
    pub entries: Vec<Entry>,
}

impl Node {
    pub fn is_leaf(&self) -> bool {
        self.level == 0
    }

    /// Read and parse a node from disk, verifying magic and CRC.
    pub fn read(io: &dyn BlockIO, block: BlockNum) -> Result<Self, FsError> {
        let mut buf = BlockBuf::zeroed();
        io.read_block(block, &mut buf);
        Self::parse(&buf, block)
    }

    fn parse(buf: &BlockBuf, block: BlockNum) -> Result<Self, FsError> {
        let b = buf.as_bytes();

        let magic = [b[0], b[1], b[2], b[3]];
        if magic != NODE_MAGIC {
            return Err(FsError::BadMagic {
                expected: NODE_MAGIC,
                got: magic,
            });
        }

        let stored_crc = u32::from_le_bytes(b[4..8].try_into().unwrap());
        let computed_crc = crc32c(&b[CRC_START..]);
        if stored_crc != computed_crc {
            return Err(FsError::ChecksumMismatch {
                block,
                stored: stored_crc,
                computed: computed_crc,
            });
        }

        let level = u16::from_le_bytes(b[8..10].try_into().unwrap());
        let entry_count = u16::from_le_bytes(b[10..12].try_into().unwrap()) as usize;

        let mut entries = Vec::with_capacity(entry_count);
        let mut offset = NODE_HEADER_SIZE;

        for _ in 0..entry_count {
            if offset + KEY_HEADER_SIZE > BLOCK_SIZE {
                return Err(FsError::CorruptedNode(block));
            }

            let name_hash = u64::from_le_bytes(b[offset..offset + 8].try_into().unwrap());
            let name_hash_hi =
                u64::from_le_bytes(b[offset + 8..offset + 16].try_into().unwrap());
            let key_type_raw =
                u16::from_le_bytes(b[offset + 16..offset + 18].try_into().unwrap());
            let val_len =
                u32::from_le_bytes(b[offset + 18..offset + 22].try_into().unwrap()) as usize;

            let key_type = KeyType::try_from(key_type_raw)?;

            let val_start = offset + KEY_HEADER_SIZE;
            let val_end = val_start + val_len;
            if val_end > BLOCK_SIZE {
                return Err(FsError::CorruptedNode(block));
            }

            let value = b[val_start..val_end].to_vec();
            let entry = Entry {
                key: Key {
                    name_hash,
                    name_hash_hi,
                    key_type,
                },
                value,
            };

            // Advance to next 8-byte aligned position
            offset = (val_end + 7) & !7;
            entries.push(entry);
        }

        Ok(Self { level, entries })
    }

    /// Compute how many bytes this node's entries use on disk.
    fn entries_size(&self) -> usize {
        self.entries.iter().map(|e| e.disk_size()).sum()
    }

    /// Check if an entry with the given disk_size can fit.
    pub fn can_fit(&self, entry_disk_size: usize) -> bool {
        NODE_HEADER_SIZE + self.entries_size() + entry_disk_size <= BLOCK_SIZE
    }

    /// Serialize this node to a block buffer, computing the CRC.
    pub fn write_to(&self, buf: &mut BlockBuf) {
        let b = buf.as_bytes_mut();
        b.fill(0);

        b[0..4].copy_from_slice(&NODE_MAGIC);
        // CRC at [4..8] filled last
        b[8..10].copy_from_slice(&self.level.to_le_bytes());
        b[10..12].copy_from_slice(&(self.entries.len() as u16).to_le_bytes());

        let used = self.entries_size();
        let free_space = (MAX_PAYLOAD - used) as u32;
        b[12..16].copy_from_slice(&free_space.to_le_bytes());

        let mut offset = NODE_HEADER_SIZE;
        for entry in &self.entries {
            b[offset..offset + 8].copy_from_slice(&entry.key.name_hash.to_le_bytes());
            b[offset + 8..offset + 16]
                .copy_from_slice(&entry.key.name_hash_hi.to_le_bytes());
            b[offset + 16..offset + 18]
                .copy_from_slice(&(entry.key.key_type as u16).to_le_bytes());
            b[offset + 18..offset + 22]
                .copy_from_slice(&(entry.value.len() as u32).to_le_bytes());
            // [22..24] reserved = 0

            let val_start = offset + KEY_HEADER_SIZE;
            b[val_start..val_start + entry.value.len()].copy_from_slice(&entry.value);

            offset = (val_start + entry.value.len() + 7) & !7;
        }

        let crc = crc32c(&b[CRC_START..]);
        b[4..8].copy_from_slice(&crc.to_le_bytes());
    }

    /// Write this node to disk.
    pub fn write(&self, io: &dyn BlockIO, block: BlockNum) {
        let mut buf = BlockBuf::zeroed();
        self.write_to(&mut buf);
        io.write_block(block, &buf);
    }
}

// --- B+ tree operations ---

/// Find the child block to descend into for a given key in an interior node.
fn find_child(node: &Node, key: &Key) -> BlockNum {
    // Interior node entries are sorted. Each entry's key is the minimum key
    // of that child subtree. Find the last entry whose key <= search key.
    // Default to first child (covers keys from -infinity to second entry's key).
    debug_assert!(!node.entries.is_empty(), "interior node has no children");
    let mut child_block =
        BlockNum::new(u64::from_le_bytes(node.entries[0].value[..8].try_into().unwrap()));
    for entry in &node.entries {
        if entry.key <= *key {
            child_block =
                BlockNum::new(u64::from_le_bytes(entry.value[..8].try_into().unwrap()));
        } else {
            break;
        }
    }
    child_block
}

/// Search the B+ tree for an exact key match. Returns the leaf entry's value.
pub fn search(io: &dyn BlockIO, root: BlockNum, root_level: u16, key: &Key) -> Result<Option<Vec<u8>>, FsError> {
    let mut block = root;
    let mut level = root_level;

    loop {
        let node = Node::read(io, block)?;

        if level == 0 {
            // Leaf node — search for exact key match
            for entry in &node.entries {
                if entry.key == *key {
                    return Ok(Some(entry.value.clone()));
                }
            }
            return Ok(None);
        }

        // Interior node — descend
        block = find_child(&node, key);
        level -= 1;
    }
}

/// Search the B+ tree for all entries with a given `name_hash`.
/// Returns all matching entries (there may be multiple due to hash collisions
/// or multiple key_types with the same hash).
pub fn search_by_hash(
    io: &dyn BlockIO,
    root: BlockNum,
    root_level: u16,
    name_hash: u64,
) -> Result<Vec<Entry>, FsError> {
    // Use the MAXIMUM possible key for this name_hash so we descend to the
    // rightmost child that could contain any entry with this hash.
    // Then scan the leaf — entries with matching name_hash will be there
    // because they sort between (name_hash, 0, Deleted) and (name_hash, MAX, MAX).
    let search_key = Key {
        name_hash,
        name_hash_hi: u64::MAX,
        key_type: KeyType::Symlink, // highest key_type value
    };

    let mut block = root;
    let mut level = root_level;

    // Descend to leaf
    loop {
        let node = Node::read(io, block)?;
        if level == 0 {
            let mut results = Vec::new();
            for entry in &node.entries {
                if entry.key.name_hash == name_hash && entry.key.key_type != KeyType::Deleted {
                    results.push(entry.clone());
                }
            }
            return Ok(results);
        }
        block = find_child(&node, &search_key);
        level -= 1;
    }
}

/// Delete an exact key from the B+ tree. Returns the old value if found.
/// Does not merge underflowing nodes — just removes the entry from the leaf.
pub fn delete(io: &dyn BlockIO, root: BlockNum, root_level: u16, key: &Key) -> Result<Option<Vec<u8>>, FsError> {
    let mut block = root;
    let mut level = root_level;

    // Descend to leaf
    loop {
        let mut node = Node::read(io, block)?;
        if level == 0 {
            if let Some(pos) = node.entries.iter().position(|e| e.key == *key) {
                let old = node.entries.remove(pos);
                node.write(io, block);
                return Ok(Some(old.value));
            }
            return Ok(None);
        }
        block = find_child(&node, key);
        level -= 1;
    }
}

/// Delete all entries matching a predicate by scanning the entire tree.
/// Returns the number of entries deleted.
pub fn delete_matching(
    io: &dyn BlockIO,
    root: BlockNum,
    root_level: u16,
    predicate: &dyn Fn(&Entry) -> bool,
) -> Result<usize, FsError> {
    let mut count = 0;
    delete_matching_recursive(io, root, root_level, predicate, &mut count)?;
    Ok(count)
}

fn delete_matching_recursive(
    io: &dyn BlockIO,
    block: BlockNum,
    level: u16,
    predicate: &dyn Fn(&Entry) -> bool,
    count: &mut usize,
) -> Result<(), FsError> {
    let mut node = Node::read(io, block)?;
    if level == 0 {
        let before = node.entries.len();
        node.entries.retain(|e| !predicate(e));
        let removed = before - node.entries.len();
        if removed > 0 {
            *count += removed;
            node.write(io, block);
        }
    } else {
        // Collect child block numbers first (to avoid borrowing issues)
        let children: Vec<BlockNum> = node.entries.iter()
            .map(|e| BlockNum::new(u64::from_le_bytes(e.value[..8].try_into().unwrap())))
            .collect();
        for child in children {
            delete_matching_recursive(io, child, level - 1, predicate, count)?;
        }
    }
    Ok(())
}

/// Collect all leaf entries by iterating the entire tree.
pub fn collect_all(io: &dyn BlockIO, root: BlockNum, root_level: u16) -> Result<Vec<Entry>, FsError> {
    let mut results = Vec::new();
    collect_recursive(io, root, root_level, &mut results)?;
    Ok(results)
}

fn collect_recursive(
    io: &dyn BlockIO,
    block: BlockNum,
    level: u16,
    results: &mut Vec<Entry>,
) -> Result<(), FsError> {
    let node = Node::read(io, block)?;

    if level == 0 {
        for entry in node.entries {
            if entry.key.key_type != KeyType::Deleted {
                results.push(entry);
            }
        }
    } else {
        for entry in &node.entries {
            let child =
                BlockNum::new(u64::from_le_bytes(entry.value[..8].try_into().unwrap()));
            collect_recursive(io, child, level - 1, results)?;
        }
    }
    Ok(())
}

/// Insert a key-value pair into the B+ tree.
///
/// Returns the (possibly new) root block and root level if the root was split.
pub fn insert(
    io: &dyn BlockIO,
    alloc: &mut BitmapAllocator,
    root: BlockNum,
    root_level: u16,
    entry: Entry,
) -> Result<(BlockNum, u16), FsError> {
    let split = insert_recursive(io, alloc, root, root_level, entry)?;

    match split {
        InsertResult::Done => Ok((root, root_level)),
        InsertResult::Split { new_block, split_key } => {
            // Root was split — create new root
            let new_root_block = alloc.alloc_block(io)?;
            let old_min_key = find_min_key(io, root, root_level)?;

            let mut new_root = Node {
                level: root_level + 1,
                entries: Vec::with_capacity(2),
            };
            new_root.entries.push(Entry {
                key: old_min_key,
                value: root.raw().to_le_bytes().to_vec(),
            });
            new_root.entries.push(Entry {
                key: split_key,
                value: new_block.raw().to_le_bytes().to_vec(),
            });
            new_root.write(io, new_root_block);

            Ok((new_root_block, root_level + 1))
        }
    }
}

enum InsertResult {
    Done,
    Split {
        new_block: BlockNum,
        split_key: Key,
    },
}

fn insert_recursive(
    io: &dyn BlockIO,
    alloc: &mut BitmapAllocator,
    block: BlockNum,
    level: u16,
    entry: Entry,
) -> Result<InsertResult, FsError> {
    let mut node = Node::read(io, block)?;

    if level == 0 {
        // Leaf — insert into sorted position, replacing if key matches
        let pos = match node.entries.binary_search_by(|e| e.key.cmp(&entry.key)) {
            Ok(i) => {
                // Replace existing
                node.entries[i] = entry.clone();
                None
            }
            Err(i) => {
                node.entries.insert(i, entry.clone());
                Some(i)
            }
        };

        if node.can_fit(0) || (pos.is_none() && node.entries_size() <= MAX_PAYLOAD) {
            // Check total entries fit
            if NODE_HEADER_SIZE + node.entries_size() <= BLOCK_SIZE {
                node.write(io, block);
                return Ok(InsertResult::Done);
            }
        }

        // Need to split
        return split_node(io, alloc, block, node);
    }

    // Interior node — find child and recurse
    let child_idx = {
        let mut idx = 0;
        for (i, e) in node.entries.iter().enumerate() {
            if e.key <= entry.key {
                idx = i;
            } else {
                break;
            }
        }
        idx
    };

    let child_block =
        BlockNum::new(u64::from_le_bytes(node.entries[child_idx].value[..8].try_into().unwrap()));

    let result = insert_recursive(io, alloc, child_block, level - 1, entry)?;

    match result {
        InsertResult::Done => Ok(InsertResult::Done),
        InsertResult::Split { new_block, split_key } => {
            // Child was split — insert new pointer into this interior node
            let new_entry = Entry {
                key: split_key,
                value: new_block.raw().to_le_bytes().to_vec(),
            };

            let pos = match node.entries.binary_search_by(|e| e.key.cmp(&new_entry.key)) {
                Ok(i) => i + 1,
                Err(i) => i,
            };
            node.entries.insert(pos, new_entry);

            if NODE_HEADER_SIZE + node.entries_size() <= BLOCK_SIZE {
                node.write(io, block);
                Ok(InsertResult::Done)
            } else {
                split_node(io, alloc, block, node)
            }
        }
    }
}

fn split_node(
    io: &dyn BlockIO,
    alloc: &mut BitmapAllocator,
    block: BlockNum,
    mut node: Node,
) -> Result<InsertResult, FsError> {
    let mid = node.entries.len() / 2;
    let right_entries: Vec<Entry> = node.entries.drain(mid..).collect();
    let split_key = right_entries[0].key;

    let right_block = alloc.alloc_block(io)?;
    let right_node = Node {
        level: node.level,
        entries: right_entries,
    };

    // Write both halves
    node.write(io, block);
    right_node.write(io, right_block);

    Ok(InsertResult::Split {
        new_block: right_block,
        split_key,
    })
}

/// Find the minimum key in a subtree.
fn find_min_key(io: &dyn BlockIO, block: BlockNum, level: u16) -> Result<Key, FsError> {
    let node = Node::read(io, block)?;
    if node.entries.is_empty() {
        return Ok(Key::ZERO);
    }
    if level == 0 {
        Ok(node.entries[0].key)
    } else {
        let child =
            BlockNum::new(u64::from_le_bytes(node.entries[0].value[..8].try_into().unwrap()));
        find_min_key(io, child, level - 1)
    }
}
