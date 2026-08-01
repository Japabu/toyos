use crate::block_io::{BlockBuf, BlockNum, BlockIO, BLOCK_SIZE};
use crate::crc32c::crc32c;
use crate::fs::FsError;

pub const MAGIC: [u8; 4] = *b"BCFS";
pub const VERSION: u32 = 1;

/// The designation stamp: the *only* thing that authorises ToyOS to destroy
/// what is on a block device.
///
/// Not a bcachefs concept, and it lives here anyway, because it occupies the
/// same block as the superblock and the one invariant that matters is that
/// the two can never be confused — which is checkable at a glance only if
/// they sit next to each other. The assertion below is the check.
///
/// Bytes 0..16 are this magic and bytes 16..24 the little-endian block count
/// of the device the stamp designates, so a stamped image copied onto a
/// different disk designates nothing. The rest of the block is ignored.
///
/// Reading it is safe on any disk; writing it is the act of designation, and
/// it destroys whatever partition table was there. That is the point: there
/// is no way to designate a disk by accident, and no way to do it without
/// having already decided to lose its contents.
pub const DESIGNATION_MAGIC: [u8; 16] = *b"TOYOS-FORMAT-ME\0";

/// Bytes 16..24 of a designation stamp.
pub const DESIGNATION_BLOCKS_OFFSET: usize = 16;

const _: () = assert!(
    DESIGNATION_MAGIC[0] != MAGIC[0]
        || DESIGNATION_MAGIC[1] != MAGIC[1]
        || DESIGNATION_MAGIC[2] != MAGIC[2]
        || DESIGNATION_MAGIC[3] != MAGIC[3],
    "a designation stamp would parse as a superblock, or the reverse",
);

/// On-disk superblock layout. Stored at block 0 and backed up at the last block.
#[derive(Debug, Clone)]
pub struct Superblock {
    pub block_count: u64,
    pub root_node: BlockNum,
    pub next_alloc: u64,
    pub free_blocks: u64,
    pub bitmap_start: BlockNum,
    pub bitmap_blocks: u64,
    pub journal_start: BlockNum,
    pub journal_blocks: u32,
    pub journal_head: u64,
    pub flags: u16,
    pub hash_seed: [u8; 16],
}

impl Superblock {
    pub(crate) const CRC_START: usize = 12; // CRC covers bytes [12..4096]

    pub fn is_clean(&self) -> bool {
        self.flags & 1 != 0
    }

    pub fn set_clean(&mut self, clean: bool) {
        if clean {
            self.flags |= 1;
        } else {
            self.flags &= !1;
        }
    }

    /// Parse a superblock from a block buffer. Verifies magic, version, and CRC.
    ///
    /// The envelope only. Every field below is a number the disk chose, and a
    /// CRC is not authentication — whoever writes the image writes the CRC.
    /// [`Superblock::read`] is the entry point that also refuses a superblock
    /// describing a device other than the one it came off, and it is the only
    /// road to a mount.
    pub fn parse(buf: &BlockBuf) -> Result<Self, FsError> {
        let b = buf.as_bytes();

        let magic = [b[0], b[1], b[2], b[3]];
        if magic != MAGIC {
            return Err(FsError::BadMagic { expected: MAGIC, got: magic });
        }

        let version = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
        if version != VERSION {
            return Err(FsError::UnsupportedVersion(version));
        }

        let stored_crc = u32::from_le_bytes([b[8], b[9], b[10], b[11]]);
        let computed_crc = crc32c(&b[Self::CRC_START..]);
        if stored_crc != computed_crc {
            return Err(FsError::ChecksumMismatch {
                block: BlockNum::new(0),
                stored: stored_crc,
                computed: computed_crc,
            });
        }

        let mut hash_seed = [0u8; 16];
        hash_seed.copy_from_slice(&b[90..106]);

        Ok(Self {
            block_count: read_u64(b, 12),
            root_node: BlockNum::new(read_u64(b, 24)),
            next_alloc: read_u64(b, 36),
            free_blocks: read_u64(b, 44),
            bitmap_start: BlockNum::new(read_u64(b, 52)),
            bitmap_blocks: read_u64(b, 60),
            journal_start: BlockNum::new(read_u64(b, 68)),
            journal_blocks: read_u32(b, 76),
            journal_head: read_u64(b, 80),
            flags: read_u16(b, 88),
            hash_seed,
        })
    }

    /// Serialize the superblock into a block buffer, computing the CRC.
    pub fn write_to(&self, buf: &mut BlockBuf) {
        let b = buf.as_bytes_mut();
        b.fill(0);

        b[0..4].copy_from_slice(&MAGIC);
        write_u32(b, 4, VERSION);
        // CRC at [8..12] filled last

        write_u64(b, 12, self.block_count);
        write_u32(b, 20, BLOCK_SIZE as u32);
        write_u64(b, 24, self.root_node.raw());
        // [32..36] pad — the tree's depth used to live here, and drove three
        // recursions against a 128 KiB kernel stack. The descent ends at a
        // `Node::Leaf` now, so the disk has no say in how deep it goes.
        write_u64(b, 36, self.next_alloc);
        write_u64(b, 44, self.free_blocks);
        write_u64(b, 52, self.bitmap_start.raw());
        write_u64(b, 60, self.bitmap_blocks);
        write_u64(b, 68, self.journal_start.raw());
        write_u32(b, 76, self.journal_blocks);
        write_u64(b, 80, self.journal_head);
        write_u16(b, 88, self.flags);
        b[90..106].copy_from_slice(&self.hash_seed);

        let crc = crc32c(&b[Self::CRC_START..]);
        write_u32(b, 8, crc);
    }

    /// Refuse a superblock that does not describe the device it was read from.
    ///
    /// Nine of these fields are indices into a device whose size the
    /// superblock does not get to declare, and `Mounted::open` copied five of
    /// them straight into the allocator. An unchecked `bitmap_start` puts
    /// bitmap writes on arbitrary blocks; an unchecked `block_count` above the
    /// device puts the backup superblock past the end of it.
    fn check(&self, device_blocks: u64) -> Result<(), FsError> {
        let bad = |field| Err(FsError::BadSuperblock { field });

        if self.block_count == 0 || self.block_count > device_blocks {
            return bad("block_count");
        }
        if self.root_node.raw() >= self.block_count {
            return bad("root_node");
        }
        // Block 0 is the superblock, so a bitmap starting there overwrites it.
        if self.bitmap_start.raw() == 0 || self.bitmap_start.raw() >= self.block_count {
            return bad("bitmap_start");
        }
        let bitmap_needed = self.block_count.div_ceil(BLOCK_SIZE as u64 * 8);
        let bitmap_end = self.bitmap_start.raw().checked_add(self.bitmap_blocks);
        if self.bitmap_blocks < bitmap_needed
            || bitmap_end.is_none_or(|end| end > self.block_count)
        {
            return bad("bitmap_blocks");
        }
        if self.journal_start.raw() >= self.block_count {
            return bad("journal_start");
        }
        if self.free_blocks > self.block_count {
            return bad("free_blocks");
        }
        if self.next_alloc >= self.block_count {
            return bad("next_alloc");
        }
        Ok(())
    }

    /// Read superblock from disk, trying block 0 first, then backup at last block.
    pub fn read(io: &dyn BlockIO) -> Result<Self, FsError> {
        let device_blocks = io.block_count();
        if device_blocks == 0 {
            return Err(FsError::BadSuperblock { field: "device has no blocks" });
        }

        let checked = |buf: &BlockBuf| {
            Self::parse(buf).and_then(|sb| sb.check(device_blocks).map(|()| sb))
        };

        let mut buf = BlockBuf::zeroed();
        io.read_block(BlockNum::new(0), &mut buf);
        match checked(&buf) {
            Ok(sb) => Ok(sb),
            Err(primary_err) => {
                io.read_block(BlockNum::new(device_blocks - 1), &mut buf);
                checked(&buf).map_err(|_| primary_err)
            }
        }
    }

    /// Write superblock to both block 0 and the backup at the last block.
    pub fn write(&self, io: &dyn BlockIO) {
        let mut buf = BlockBuf::zeroed();
        self.write_to(&mut buf);
        io.write_block(BlockNum::new(0), &buf);
        let last = BlockNum::new(self.block_count - 1);
        io.write_block(last, &buf);
    }
}

// --- Little-endian helpers ---

fn read_u16(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([buf[off], buf[off + 1]])
}

fn read_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}

fn read_u64(buf: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(buf[off..off + 8].try_into().unwrap())
}

fn write_u16(buf: &mut [u8], off: usize, val: u16) {
    buf[off..off + 2].copy_from_slice(&val.to_le_bytes());
}

fn write_u32(buf: &mut [u8], off: usize, val: u32) {
    buf[off..off + 4].copy_from_slice(&val.to_le_bytes());
}

fn write_u64(buf: &mut [u8], off: usize, val: u64) {
    buf[off..off + 8].copy_from_slice(&val.to_le_bytes());
}
