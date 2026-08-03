use alloc::vec::Vec;
use crate::block_io::{BlockBuf, BlockNum, BlockIO, BlockIOExt, BLOCK_SIZE};
use crate::crc32c::crc32c;
use crate::alloc_bitmap::BitmapAllocator;
use crate::fs::FsError;

pub const NODE_MAGIC: [u8; 4] = *b"BTND";
const NODE_HEADER_SIZE: usize = 32;
const KEY_HEADER_SIZE: usize = 24;
pub(crate) const CRC_START: usize = 8; // CRC covers bytes [8..4096]
const MAX_PAYLOAD: usize = BLOCK_SIZE - NODE_HEADER_SIZE;

/// A child pointer is one block number, and nothing else ever appears in an
/// interior node's value.
const CHILD_VALUE_SIZE: usize = 8;
const CHILD_DISK_SIZE: usize = (KEY_HEADER_SIZE + CHILD_VALUE_SIZE + 7) & !7;

/// The most entries a 4096-byte block can physically hold.
///
/// The count is a `u16` on disk with 387 times this range, and it used to size
/// a `Vec` before a single byte of the block had been looked at.
const MAX_ENTRIES: usize = MAX_PAYLOAD / KEY_HEADER_SIZE;

/// The largest `Entry::disk_size` any node can ever hold.
///
/// A node holds at least one entry, so an entry bigger than this cannot be
/// stored at all — and splitting does not help: a split of one oversized entry
/// leaves an empty left node and a right node that is still oversized. It has
/// to be refused at the door, before anything is written, because every caller
/// that builds a value builds it out of something userland chose (a name, or
/// one extent per discontiguous run of a file).
pub const MAX_ENTRY_SIZE: usize = MAX_PAYLOAD;

/// Total on-disk size of a leaf holding these entries.
fn leaf_size(entries: &[Entry]) -> usize {
    NODE_HEADER_SIZE + entries.iter().map(|e| e.disk_size()).sum::<usize>()
}

/// Reject an entry no node could hold. Call before mutating anything.
pub fn check_entry_fits(entry: &Entry) -> Result<(), FsError> {
    let size = entry.disk_size();
    if size > MAX_ENTRY_SIZE {
        return Err(FsError::EntryTooLarge { size, max: MAX_ENTRY_SIZE });
    }
    Ok(())
}

/// How much further a descent is allowed to go.
///
/// The shape of the tree is on the disk, and a disk is not ours. A child
/// pointer naming its own node, or any cycle at all, is a descent that never
/// reaches a leaf; nothing in the block format forbids one, so the bound is
/// carried by the only operation that can go deeper.
#[derive(Clone, Copy)]
struct Depth(u8);

impl Depth {
    /// A B+ tree whose interior nodes hold at least two children is at most
    /// `log2(blocks)` deep, and a block number is a `u64`.
    const ROOT: Self = Self(64);

    fn descend(self, from: BlockNum) -> Result<Self, FsError> {
        match self.0.checked_sub(1) {
            Some(left) => Ok(Self(left)),
            None => Err(FsError::TreeTooDeep(from)),
        }
    }
}

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

/// A key-value entry in a B+ tree leaf.
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

/// A child pointer: the minimum key of the subtree, and the block it lives in.
#[derive(Debug, Clone, Copy)]
pub struct Child {
    pub key: Key,
    pub block: BlockNum,
}

/// A parsed B+ tree node.
///
/// The variant *is* the leaf/interior distinction. It is decided once, by
/// [`Node::parse`], which is also where a child pointer is turned into a
/// `BlockNum` — checked to be eight bytes long and to name a block that exists
/// on the device. Six descent sites used to do that decode themselves, from a
/// value whose length nothing had constrained.
///
/// `level` rides along so the header round-trips; the only arithmetic on it is
/// the checked increment that gives a new root its level.
pub enum Node {
    Leaf(Vec<Entry>),
    /// `children` is never empty: an interior node with no children is a
    /// subtree with no bottom.
    Interior { level: u16, children: Vec<Child> },
}

impl Node {
    fn level(&self) -> u16 {
        match self {
            Node::Leaf(_) => 0,
            Node::Interior { level, .. } => *level,
        }
    }

    fn count(&self) -> usize {
        match self {
            Node::Leaf(entries) => entries.len(),
            Node::Interior { children, .. } => children.len(),
        }
    }

    /// Bytes this node's entries occupy after the header.
    fn payload_size(&self) -> usize {
        match self {
            Node::Leaf(entries) => entries.iter().map(|e| e.disk_size()).sum(),
            Node::Interior { children, .. } => children.len() * CHILD_DISK_SIZE,
        }
    }

    /// Read and parse a node from disk, verifying magic and CRC.
    pub fn read(io: &dyn BlockIO, block: BlockNum) -> Result<Self, FsError> {
        let device_blocks = io.block_count();
        let mut buf = BlockBuf::zeroed();
        io.read(block, &mut buf)?;
        Self::parse(&buf, block, device_blocks)
    }

    fn parse(buf: &BlockBuf, block: BlockNum, device_blocks: u64) -> Result<Self, FsError> {
        let b = buf.as_bytes();

        let magic = [b[0], b[1], b[2], b[3]];
        if magic != NODE_MAGIC {
            return Err(FsError::BadMagic {
                expected: NODE_MAGIC,
                got: magic,
            });
        }

        let stored_crc = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
        let computed_crc = crc32c(&b[CRC_START..]);
        if stored_crc != computed_crc {
            return Err(FsError::ChecksumMismatch {
                block,
                stored: stored_crc,
                computed: computed_crc,
            });
        }

        let level = u16::from_le_bytes([b[8], b[9]]);
        let entry_count = u16::from_le_bytes([b[10], b[11]]) as usize;
        if entry_count > MAX_ENTRIES {
            return Err(FsError::CorruptedNode(block));
        }

        // Grown, not reserved: `entry_count` is a number off the disk, and
        // reserving from it asked the kernel allocator for 3,145,680 bytes
        // against a 2 MiB ceiling. What the block can actually hold is what
        // gets allocated, and the refusal above is the fail-fast on top.
        let mut entries: Vec<Entry> = Vec::new();
        let mut offset = NODE_HEADER_SIZE;

        for _ in 0..entry_count {
            if offset + KEY_HEADER_SIZE > BLOCK_SIZE {
                return Err(FsError::CorruptedNode(block));
            }

            let name_hash = read_u64(b, offset);
            let name_hash_hi = read_u64(b, offset + 8);
            let key_type_raw = u16::from_le_bytes([b[offset + 16], b[offset + 17]]);
            let val_len = u32::from_le_bytes([
                b[offset + 18],
                b[offset + 19],
                b[offset + 20],
                b[offset + 21],
            ]) as usize;

            let key_type = KeyType::try_from(key_type_raw)?;

            let val_start = offset + KEY_HEADER_SIZE;
            let val_end = val_start + val_len;
            if val_end > BLOCK_SIZE {
                return Err(FsError::CorruptedNode(block));
            }

            entries.push(Entry {
                key: Key { name_hash, name_hash_hi, key_type },
                value: b[val_start..val_end].to_vec(),
            });

            offset = (val_end + 7) & !7;
        }

        if level == 0 {
            return Ok(Node::Leaf(entries));
        }
        if entries.is_empty() {
            return Err(FsError::CorruptedNode(block));
        }

        let mut children = Vec::with_capacity(entries.len());
        for entry in &entries {
            let raw = entry
                .value
                .get(..CHILD_VALUE_SIZE)
                .ok_or(FsError::CorruptedNode(block))?;
            let mut bytes = [0u8; CHILD_VALUE_SIZE];
            bytes.copy_from_slice(raw);
            let child = u64::from_le_bytes(bytes);
            if child >= device_blocks {
                return Err(FsError::BlockOffDevice { block: child, device_blocks });
            }
            children.push(Child { key: entry.key, block: BlockNum::new(child) });
        }
        Ok(Node::Interior { level, children })
    }

    /// Serialize this node to a block buffer, computing the CRC.
    ///
    /// Fallible rather than asserting: `used` is a sum over values whose size
    /// userland chooses, so an overfull node is an input to reject, not a
    /// kernel bug to scream about. This bound is also what keeps every slice
    /// index below sound — the final offset is `NODE_HEADER_SIZE + used`.
    pub fn write_to(&self, buf: &mut BlockBuf) -> Result<(), FsError> {
        let used = self.payload_size();
        if used > MAX_PAYLOAD {
            return Err(FsError::NodeOverfull { used, max: MAX_PAYLOAD });
        }

        let b = buf.as_bytes_mut();
        b.fill(0);

        b[0..4].copy_from_slice(&NODE_MAGIC);
        // CRC at [4..8] filled last
        b[8..10].copy_from_slice(&self.level().to_le_bytes());
        b[10..12].copy_from_slice(&(self.count() as u16).to_le_bytes());

        let free_space = (MAX_PAYLOAD - used) as u32;
        b[12..16].copy_from_slice(&free_space.to_le_bytes());

        let mut offset = NODE_HEADER_SIZE;
        match self {
            Node::Leaf(entries) => {
                for entry in entries {
                    offset = write_entry(b, offset, &entry.key, &entry.value);
                }
            }
            Node::Interior { children, .. } => {
                for child in children {
                    offset = write_entry(b, offset, &child.key, &child.block.raw().to_le_bytes());
                }
            }
        }

        let crc = crc32c(&b[CRC_START..]);
        b[4..8].copy_from_slice(&crc.to_le_bytes());
        Ok(())
    }

    /// Write this node to disk.
    pub fn write(&self, io: &dyn BlockIO, block: BlockNum) -> Result<(), FsError> {
        let mut buf = BlockBuf::zeroed();
        self.write_to(&mut buf)?;
        io.write(block, &buf)?;
        Ok(())
    }
}

fn read_u64(b: &[u8; BLOCK_SIZE], off: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&b[off..off + 8]);
    u64::from_le_bytes(bytes)
}

/// Write one key header and its value, returning the next entry's offset.
fn write_entry(b: &mut [u8; BLOCK_SIZE], offset: usize, key: &Key, value: &[u8]) -> usize {
    b[offset..offset + 8].copy_from_slice(&key.name_hash.to_le_bytes());
    b[offset + 8..offset + 16].copy_from_slice(&key.name_hash_hi.to_le_bytes());
    b[offset + 16..offset + 18].copy_from_slice(&(key.key_type as u16).to_le_bytes());
    b[offset + 18..offset + 22].copy_from_slice(&(value.len() as u32).to_le_bytes());
    // [22..24] reserved = 0

    let val_start = offset + KEY_HEADER_SIZE;
    b[val_start..val_start + value.len()].copy_from_slice(value);
    (val_start + value.len() + 7) & !7
}

// --- B+ tree operations ---

/// The child to descend into for `key`.
///
/// Interior children are sorted and each key is the minimum key of that
/// child's subtree, so the answer is the last child whose key is `<= key`,
/// defaulting to the first — which covers everything below the second key.
fn find_child(children: &[Child], key: &Key) -> Option<BlockNum> {
    let mut chosen = children.first()?.block;
    for child in children {
        if child.key <= *key {
            chosen = child.block;
        } else {
            break;
        }
    }
    Some(chosen)
}

/// Search the B+ tree for an exact key match. Returns the leaf entry's value.
pub fn search(io: &dyn BlockIO, root: BlockNum, key: &Key) -> Result<Option<Vec<u8>>, FsError> {
    let mut block = root;
    let mut depth = Depth::ROOT;

    loop {
        match Node::read(io, block)? {
            Node::Leaf(entries) => {
                return Ok(entries.into_iter().find(|e| e.key == *key).map(|e| e.value));
            }
            Node::Interior { children, .. } => {
                let next = find_child(&children, key).ok_or(FsError::CorruptedNode(block))?;
                depth = depth.descend(block)?;
                block = next;
            }
        }
    }
}

/// Search the B+ tree for all entries with a given `name_hash`.
/// Returns all matching entries (there may be multiple due to hash collisions
/// or multiple key_types with the same hash).
pub fn search_by_hash(
    io: &dyn BlockIO,
    root: BlockNum,
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
    let mut depth = Depth::ROOT;

    loop {
        match Node::read(io, block)? {
            Node::Leaf(entries) => {
                return Ok(entries
                    .into_iter()
                    .filter(|e| e.key.name_hash == name_hash && e.key.key_type != KeyType::Deleted)
                    .collect());
            }
            Node::Interior { children, .. } => {
                let next =
                    find_child(&children, &search_key).ok_or(FsError::CorruptedNode(block))?;
                depth = depth.descend(block)?;
                block = next;
            }
        }
    }
}

/// Delete an exact key from the B+ tree. Returns the old value if found.
/// Does not merge underflowing nodes — just removes the entry from the leaf.
pub fn delete(io: &dyn BlockIO, root: BlockNum, key: &Key) -> Result<Option<Vec<u8>>, FsError> {
    let mut block = root;
    let mut depth = Depth::ROOT;

    loop {
        match Node::read(io, block)? {
            Node::Leaf(mut entries) => {
                let Some(pos) = entries.iter().position(|e| e.key == *key) else {
                    return Ok(None);
                };
                let old = entries.remove(pos);
                Node::Leaf(entries).write(io, block)?;
                return Ok(Some(old.value));
            }
            Node::Interior { children, .. } => {
                let next = find_child(&children, key).ok_or(FsError::CorruptedNode(block))?;
                depth = depth.descend(block)?;
                block = next;
            }
        }
    }
}

/// Collect all leaf entries by iterating the entire tree.
pub fn collect_all(io: &dyn BlockIO, root: BlockNum) -> Result<Vec<Entry>, FsError> {
    let mut results = Vec::new();
    collect_recursive(io, root, Depth::ROOT, &mut results)?;
    Ok(results)
}

fn collect_recursive(
    io: &dyn BlockIO,
    block: BlockNum,
    depth: Depth,
    results: &mut Vec<Entry>,
) -> Result<(), FsError> {
    match Node::read(io, block)? {
        Node::Leaf(entries) => {
            results.extend(entries.into_iter().filter(|e| e.key.key_type != KeyType::Deleted));
        }
        Node::Interior { children, .. } => {
            let deeper = depth.descend(block)?;
            for child in children {
                collect_recursive(io, child.block, deeper, results)?;
            }
        }
    }
    Ok(())
}

/// Insert a key-value pair into the B+ tree.
///
/// Returns the root block, which changes when the old root was split.
pub fn insert(
    io: &dyn BlockIO,
    alloc: &mut BitmapAllocator,
    root: BlockNum,
    entry: Entry,
) -> Result<BlockNum, FsError> {
    check_entry_fits(&entry)?;

    match insert_recursive(io, alloc, root, Depth::ROOT, entry)? {
        InsertResult::Done => Ok(root),
        InsertResult::Split { new_block, split_key } => {
            let level = Node::read(io, root)?
                .level()
                .checked_add(1)
                .ok_or(FsError::CorruptedNode(root))?;
            let old_min_key = min_key(io, root, Depth::ROOT)?;
            let new_root_block = alloc.alloc_block(io)?;

            let new_root = Node::Interior {
                level,
                children: alloc::vec![
                    Child { key: old_min_key, block: root },
                    Child { key: split_key, block: new_block },
                ],
            };
            new_root.write(io, new_root_block)?;

            Ok(new_root_block)
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
    depth: Depth,
    entry: Entry,
) -> Result<InsertResult, FsError> {
    match Node::read(io, block)? {
        Node::Leaf(mut entries) => {
            match entries.binary_search_by(|e| e.key.cmp(&entry.key)) {
                Ok(i) => entries[i] = entry,
                Err(i) => entries.insert(i, entry),
            }
            write_or_split(io, alloc, block, Node::Leaf(entries))
        }
        Node::Interior { level, children } => {
            let mut idx = 0;
            for (i, child) in children.iter().enumerate() {
                if child.key <= entry.key {
                    idx = i;
                } else {
                    break;
                }
            }
            let child_block = children.get(idx).ok_or(FsError::CorruptedNode(block))?.block;
            let deeper = depth.descend(block)?;

            match insert_recursive(io, alloc, child_block, deeper, entry)? {
                InsertResult::Done => Ok(InsertResult::Done),
                InsertResult::Split { new_block, split_key } => {
                    let mut children = children;
                    let pos = match children.binary_search_by(|c| c.key.cmp(&split_key)) {
                        Ok(i) => i + 1,
                        Err(i) => i,
                    };
                    children.insert(pos, Child { key: split_key, block: new_block });
                    write_or_split(io, alloc, block, Node::Interior { level, children })
                }
            }
        }
    }
}

fn write_or_split(
    io: &dyn BlockIO,
    alloc: &mut BitmapAllocator,
    block: BlockNum,
    node: Node,
) -> Result<InsertResult, FsError> {
    if NODE_HEADER_SIZE + node.payload_size() <= BLOCK_SIZE {
        node.write(io, block)?;
        return Ok(InsertResult::Done);
    }
    split_node(io, alloc, block, node)
}

fn split_node(
    io: &dyn BlockIO,
    alloc: &mut BitmapAllocator,
    block: BlockNum,
    node: Node,
) -> Result<InsertResult, FsError> {
    match node {
        Node::Leaf(mut entries) => {
            // One entry is not a split problem. Halving by *count* used to
            // produce `mid == 0` here, which drained every entry into the right
            // node and left an empty one behind — and the right node was still
            // the oversized entry.
            if entries.len() < 2 {
                let size = entries.first().map_or(0, |e| e.disk_size());
                return Err(FsError::EntryTooLarge { size, max: MAX_ENTRY_SIZE });
            }

            // By size, not by count: leaf entries are variable-length (a file's
            // extent list lives inline), so half the entries can be far more
            // than half the bytes.
            let mid = split_point(&entries);

            // Both halves are checked before either is written. A split that
            // has already replaced the left node on disk and then fails is a
            // corrupt tree; a split that fails before writing is an error the
            // caller can return.
            if leaf_size(&entries[..mid]) > BLOCK_SIZE || leaf_size(&entries[mid..]) > BLOCK_SIZE {
                // Unreachable while every entry is <= MAX_ENTRY_SIZE and the
                // node was legal before this insert, except for one shape: a
                // node of large entries where the new one lands in the middle.
                // Splitting three ways is what would fix it; extent merging is
                // what stops values getting near that size in the first place.
                return Err(FsError::NodeOverfull {
                    used: leaf_size(&entries) - NODE_HEADER_SIZE,
                    max: MAX_PAYLOAD,
                });
            }

            let right: Vec<Entry> = entries.drain(mid..).collect();
            let Some(split_key) = right.first().map(|e| e.key) else {
                return Err(FsError::CorruptedNode(block));
            };

            let right_block = alloc.alloc_block(io)?;
            Node::Leaf(entries).write(io, block)?;
            Node::Leaf(right).write(io, right_block)?;

            Ok(InsertResult::Split { new_block: right_block, split_key })
        }
        Node::Interior { level, mut children } => {
            if children.len() < 2 {
                return Err(FsError::CorruptedNode(block));
            }
            // Every child costs the same 32 bytes, so halving by count is
            // halving by bytes — the rule leaves are not allowed to use.
            let mid = children.len() / 2;
            let right: Vec<Child> = children.drain(mid..).collect();
            let Some(split_key) = right.first().map(|c| c.key) else {
                return Err(FsError::CorruptedNode(block));
            };

            let right_block = alloc.alloc_block(io)?;
            Node::Interior { level, children }.write(io, block)?;
            Node::Interior { level, children: right }.write(io, right_block)?;

            Ok(InsertResult::Split { new_block: right_block, split_key })
        }
    }
}

/// The largest prefix of `entries` that still fits in a node, clamped so both
/// sides of the split get at least one entry. Caller guarantees `len >= 2`.
fn split_point(entries: &[Entry]) -> usize {
    let mut used = NODE_HEADER_SIZE;
    let mut n = 0;
    for entry in entries {
        let next = used + entry.disk_size();
        if next > BLOCK_SIZE {
            break;
        }
        used = next;
        n += 1;
    }
    n.clamp(1, entries.len() - 1)
}

/// Find the minimum key in a subtree.
fn min_key(io: &dyn BlockIO, block: BlockNum, depth: Depth) -> Result<Key, FsError> {
    match Node::read(io, block)? {
        Node::Leaf(entries) => Ok(entries.first().map_or(Key::ZERO, |e| e.key)),
        Node::Interior { children, .. } => {
            let first = children.first().ok_or(FsError::CorruptedNode(block))?.block;
            min_key(io, first, depth.descend(block)?)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn entry(value_len: usize) -> Entry {
        Entry { key: Key::ZERO, value: vec![0u8; value_len] }
    }

    /// A block holding a node header the caller chose and no valid entries.
    /// The CRC is computed last, so every case below is a block the parser
    /// accepts as authentic — which is the point: a checksum says the bytes
    /// are the bytes somebody wrote.
    fn crafted(level: u16, entry_count: u16, entries: &[(u32, &[u8])]) -> BlockBuf {
        let mut buf = BlockBuf::zeroed();
        let b = buf.as_bytes_mut();
        b[0..4].copy_from_slice(&NODE_MAGIC);
        b[8..10].copy_from_slice(&level.to_le_bytes());
        b[10..12].copy_from_slice(&entry_count.to_le_bytes());
        let mut offset = NODE_HEADER_SIZE;
        for (declared_len, value) in entries {
            b[offset + 18..offset + 22].copy_from_slice(&declared_len.to_le_bytes());
            let val_start = offset + KEY_HEADER_SIZE;
            b[val_start..val_start + value.len()].copy_from_slice(value);
            offset = (val_start + *declared_len as usize + 7) & !7;
        }
        let crc = crc32c(&b[CRC_START..]);
        b[4..8].copy_from_slice(&crc.to_le_bytes());
        buf
    }

    #[test]
    fn split_point_is_the_largest_prefix_that_fits() {
        // Two entries that only fit apart: the rule has to put one on each
        // side, which halving by count also gets right.
        let two = [entry(3000), entry(3000)];
        assert_eq!(split_point(&two), 1);

        // And the shape it does not: a small entry ahead of two large ones.
        // Halving by count gives mid=1, leaving 6048 bytes of entries in the
        // right node and a block that cannot hold them.
        let skewed = [entry(1000), entry(3000), entry(3000)];
        let mid = split_point(&skewed);
        assert_eq!(mid, 2);
        assert!(leaf_size(&skewed[..mid]) <= BLOCK_SIZE, "left half does not fit");
        assert!(leaf_size(&skewed[mid..]) <= BLOCK_SIZE, "right half does not fit");
        assert!(leaf_size(&skewed[..2]) > BLOCK_SIZE / 2, "the shape under test is not skewed");
    }

    #[test]
    fn split_point_always_leaves_both_sides_a_entry() {
        // The clamp matters at both ends. One entry so large that no prefix
        // fits must still yield 1, not 0 — a 0 drains every entry into the
        // right node and writes an empty one back.
        let huge_first = [entry(MAX_ENTRY_SIZE), entry(16)];
        assert_eq!(split_point(&huge_first), 1);

        // And a node of entries that all fit must still give the right side
        // something, or the split makes no progress.
        let tiny = [entry(8), entry(8), entry(8)];
        let mid = split_point(&tiny);
        assert!(mid >= 1 && mid <= 2, "mid={mid} leaves a side empty");
    }

    #[test]
    fn an_entry_larger_than_the_payload_is_refused() {
        assert!(check_entry_fits(&entry(MAX_ENTRY_SIZE - KEY_HEADER_SIZE)).is_ok());
        assert!(matches!(
            check_entry_fits(&entry(MAX_ENTRY_SIZE)),
            Err(FsError::EntryTooLarge { .. }),
        ));
    }

    #[test]
    fn write_to_refuses_an_overfull_node_instead_of_underflowing() {
        let node = Node::Leaf(vec![entry(3000), entry(3000)]);
        let mut buf = BlockBuf::zeroed();
        assert!(matches!(node.write_to(&mut buf), Err(FsError::NodeOverfull { .. })));
    }

    #[test]
    fn an_interior_child_shorter_than_a_block_number_is_refused() {
        // Four bytes where a child pointer belongs. Six descent sites used to
        // index `value[..8]` here.
        let buf = crafted(1, 1, &[(4, &[1, 0, 0, 0])]);
        assert!(matches!(
            Node::parse(&buf, BlockNum::new(7), 64),
            Err(FsError::CorruptedNode(_)),
        ));
    }

    #[test]
    fn an_interior_node_with_no_children_is_refused() {
        let buf = crafted(1, 0, &[]);
        assert!(matches!(
            Node::parse(&buf, BlockNum::new(7), 64),
            Err(FsError::CorruptedNode(_)),
        ));
    }

    #[test]
    fn a_child_pointer_off_the_device_is_refused() {
        let buf = crafted(1, 1, &[(8, &u64::MAX.to_le_bytes())]);
        assert!(matches!(
            Node::parse(&buf, BlockNum::new(7), 64),
            Err(FsError::BlockOffDevice { .. }),
        ));
    }

    #[test]
    fn a_crafted_entry_count_never_reaches_the_allocator() {
        // The declared count is a `u16`; the block has room for MAX_ENTRIES.
        // Reserving from the former asked for more than the kernel's whole
        // allocation ceiling, and returned the *same* error while doing it —
        // so the peak allocation, not the return value, is the instrument.
        assert_eq!(MAX_ENTRIES, 169);
        let ceiling = 2 * 1024 * 1024 - 4096; // mm::MAX_HEAP_ALLOC
        let reserved_from_disk = u16::MAX as usize * core::mem::size_of::<Entry>();
        assert!(
            reserved_from_disk > ceiling,
            "{reserved_from_disk} bytes is under the ceiling — this test proves nothing",
        );

        let buf = crafted(0, u16::MAX, &[]);
        crate::alloc_probe::take_peak();
        let parsed = Node::parse(&buf, BlockNum::new(7), 64);
        let peak = crate::alloc_probe::take_peak();

        assert!(matches!(parsed, Err(FsError::CorruptedNode(_))));
        assert!(
            peak <= BLOCK_SIZE,
            "parsing a block that declares {} entries asked the allocator for {peak} bytes",
            u16::MAX,
        );
    }

    #[test]
    fn a_descent_gives_up_before_it_runs_out_of_stack() {
        let mut depth = Depth::ROOT;
        for _ in 0..64 {
            depth = depth.descend(BlockNum::new(1)).expect("64 levels is a legal tree");
        }
        assert!(matches!(
            depth.descend(BlockNum::new(1)),
            Err(FsError::TreeTooDeep(_)),
        ));
    }
}
