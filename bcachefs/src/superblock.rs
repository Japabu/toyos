use crate::block_io::{BlockBuf, BlockNum, BlockIO, BLOCK_SIZE};
use crate::crc32c::crc32c;
use crate::fs::FsError;

pub const MAGIC: [u8; 4] = *b"BCFS";
pub const VERSION: u32 = 1;

/// On-disk superblock layout. Stored at block 0 and backed up at the last block.
#[derive(Debug, Clone)]
pub struct Superblock {
    pub block_count: u64,
    pub root_node: BlockNum,
    pub root_level: u16,
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
    const CRC_START: usize = 12; // CRC covers bytes [12..4096]

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
            root_level: read_u16(b, 32),
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
        write_u16(b, 32, self.root_level);
        // [34..36] pad
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

    /// Read superblock from disk, trying block 0 first, then backup at last block.
    pub fn read(io: &dyn BlockIO) -> Result<Self, FsError> {
        let mut buf = BlockBuf::zeroed();
        io.read_block(BlockNum::new(0), &mut buf);
        match Self::parse(&buf) {
            Ok(sb) => Ok(sb),
            Err(primary_err) => {
                let last = BlockNum::new(io.block_count() - 1);
                io.read_block(last, &mut buf);
                Self::parse(&buf).map_err(|_| primary_err)
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
