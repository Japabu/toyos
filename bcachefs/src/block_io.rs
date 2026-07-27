use core::fmt;

pub const BLOCK_SIZE: usize = 4096;

/// A block number on disk. Cannot be confused with a byte offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockNum(u64);

impl BlockNum {
    pub const fn new(n: u64) -> Self {
        Self(n)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }

    pub const fn to_byte_offset(self) -> u64 {
        self.0 * BLOCK_SIZE as u64
    }

    pub const fn checked_add(self, n: u64) -> Option<Self> {
        match self.0.checked_add(n) {
            Some(v) => Some(Self(v)),
            None => None,
        }
    }
}

impl fmt::Display for BlockNum {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "block#{}", self.0)
    }
}

/// A 4096-byte aligned block buffer. Guarantees correct size at compile time.
#[repr(C, align(4096))]
pub struct BlockBuf(pub [u8; BLOCK_SIZE]);

impl BlockBuf {
    pub fn zeroed() -> Self {
        Self([0u8; BLOCK_SIZE])
    }

    pub fn as_bytes(&self) -> &[u8; BLOCK_SIZE] {
        &self.0
    }

    pub fn as_bytes_mut(&mut self) -> &mut [u8; BLOCK_SIZE] {
        &mut self.0
    }
}

impl Default for BlockBuf {
    fn default() -> Self {
        Self::zeroed()
    }
}

/// Block-level I/O abstraction.
///
/// `&self` with interior mutability — implementations handle their own
/// synchronization. `buf` is always exactly BLOCK_SIZE bytes via BlockBuf.
pub trait BlockIO {
    fn read_block(&self, block: BlockNum, buf: &mut BlockBuf);
    fn write_block(&self, block: BlockNum, buf: &BlockBuf);
    fn block_count(&self) -> u64;
    fn sync(&self) {}
}

// --- Host-side implementations ---

/// In-memory block device backed by a Vec<u8>. Used by mkfs on the host.
#[cfg(feature = "std")]
pub struct VecBlockIO {
    data: std::cell::RefCell<Vec<u8>>,
}

#[cfg(feature = "std")]
impl VecBlockIO {
    pub fn new(block_count: u64) -> Self {
        let size = block_count as usize * BLOCK_SIZE;
        Self {
            data: std::cell::RefCell::new(vec![0u8; size]),
        }
    }

    pub fn from_vec(data: Vec<u8>) -> Self {
        Self {
            data: std::cell::RefCell::new(data),
        }
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.data.into_inner()
    }
}

#[cfg(feature = "std")]
impl BlockIO for VecBlockIO {
    fn read_block(&self, block: BlockNum, buf: &mut BlockBuf) {
        let data = self.data.borrow();
        let off = block.raw() as usize * BLOCK_SIZE;
        buf.0.copy_from_slice(&data[off..off + BLOCK_SIZE]);
    }

    fn write_block(&self, block: BlockNum, buf: &BlockBuf) {
        let mut data = self.data.borrow_mut();
        let off = block.raw() as usize * BLOCK_SIZE;
        data[off..off + BLOCK_SIZE].copy_from_slice(&buf.0);
    }

    fn block_count(&self) -> u64 {
        (self.data.borrow().len() / BLOCK_SIZE) as u64
    }
}

/// Read-only block device backed by a static byte slice. Used for initrd in the kernel.
pub struct SliceBlockIO {
    data: *const u8,
    len: usize,
}

unsafe impl Send for SliceBlockIO {}
unsafe impl Sync for SliceBlockIO {}

impl SliceBlockIO {
    /// Create a read-only block device from a raw pointer and length.
    ///
    /// # Safety
    /// The pointer must remain valid for the lifetime of this object,
    /// and `len` must be accurate.
    pub unsafe fn new(data: *const u8, len: usize) -> Self {
        Self { data, len }
    }

    fn as_slice(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.data, self.len) }
    }
}

impl BlockIO for SliceBlockIO {
    fn read_block(&self, block: BlockNum, buf: &mut BlockBuf) {
        let data = self.as_slice();
        let off = block.raw() as usize * BLOCK_SIZE;
        buf.0.copy_from_slice(&data[off..off + BLOCK_SIZE]);
    }

    fn write_block(&self, _block: BlockNum, _buf: &BlockBuf) {
        panic!("SliceBlockIO is read-only");
    }

    fn block_count(&self) -> u64 {
        (self.len / BLOCK_SIZE) as u64
    }
}
