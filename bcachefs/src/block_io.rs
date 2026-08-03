use core::fmt;

use crate::fs::FsError;

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

/// The device did not do the transfer.
///
/// Carries nothing: which block it was is the caller's, because the caller is
/// what named it. [`BlockIOExt`] is where that gets attached, so an error a
/// filesystem operation returns cannot name a block the operation never asked
/// for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceError;

/// Block-level I/O abstraction.
///
/// `&self` with interior mutability — implementations handle their own
/// synchronization. `buf` is always exactly BLOCK_SIZE bytes via BlockBuf.
///
/// Every method is fallible for the reason every [`BlockDevice`] method is: a
/// block the device would not give back is not a block of zeros, and an
/// implementation with nowhere to report that has to invent one. The kernel's
/// did — it logged and served zeros, so a read error reached the btree as a
/// block that fails its structural checks rather than as a failure.
///
/// [`BlockDevice`]: ../../kernel/src/block.rs
pub trait BlockIO {
    #[must_use = "a refused read left the buffer holding whatever it held before"]
    fn read_block(&self, block: BlockNum, buf: &mut BlockBuf) -> Result<(), DeviceError>;
    #[must_use = "a refused write did not reach the device"]
    fn write_block(&self, block: BlockNum, buf: &BlockBuf) -> Result<(), DeviceError>;
    fn block_count(&self) -> u64;
    fn sync(&self) -> Result<(), DeviceError> {
        Ok(())
    }
}

/// The same three operations, reported as [`FsError`] with the block attached.
///
/// Every call site inside this crate goes through these rather than through
/// [`BlockIO`] directly, so the block number in the error is the one the caller
/// passed in and there is no way for an implementation to name a different one.
pub(crate) trait BlockIOExt {
    fn read(&self, block: BlockNum, buf: &mut BlockBuf) -> Result<(), FsError>;
    fn write(&self, block: BlockNum, buf: &BlockBuf) -> Result<(), FsError>;
    fn flush(&self) -> Result<(), FsError>;
}

impl<T: BlockIO + ?Sized> BlockIOExt for T {
    fn read(&self, block: BlockNum, buf: &mut BlockBuf) -> Result<(), FsError> {
        self.read_block(block, buf).map_err(|DeviceError| FsError::DeviceRead(block))
    }

    fn write(&self, block: BlockNum, buf: &BlockBuf) -> Result<(), FsError> {
        self.write_block(block, buf).map_err(|DeviceError| FsError::DeviceWrite(block))
    }

    fn flush(&self) -> Result<(), FsError> {
        self.sync().map_err(|DeviceError| FsError::DeviceSync)
    }
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
    fn read_block(&self, block: BlockNum, buf: &mut BlockBuf) -> Result<(), DeviceError> {
        let data = self.data.borrow();
        let off = block.raw() as usize * BLOCK_SIZE;
        let end = off.checked_add(BLOCK_SIZE).ok_or(DeviceError)?;
        buf.0.copy_from_slice(data.get(off..end).ok_or(DeviceError)?);
        Ok(())
    }

    fn write_block(&self, block: BlockNum, buf: &BlockBuf) -> Result<(), DeviceError> {
        let mut data = self.data.borrow_mut();
        let off = block.raw() as usize * BLOCK_SIZE;
        let end = off.checked_add(BLOCK_SIZE).ok_or(DeviceError)?;
        data.get_mut(off..end).ok_or(DeviceError)?.copy_from_slice(&buf.0);
        Ok(())
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
    fn read_block(&self, block: BlockNum, buf: &mut BlockBuf) -> Result<(), DeviceError> {
        let data = self.as_slice();
        let off = block.raw() as usize * BLOCK_SIZE;
        let end = off.checked_add(BLOCK_SIZE).ok_or(DeviceError)?;
        buf.0.copy_from_slice(data.get(off..end).ok_or(DeviceError)?);
        Ok(())
    }

    /// A refusal rather than a panic, now that there is somewhere to report it.
    /// Nothing can reach this — a slice is only ever mounted `ReadOnly`, which
    /// has no write operations — and a device that will not write is an answer
    /// either way.
    fn write_block(&self, _block: BlockNum, _buf: &BlockBuf) -> Result<(), DeviceError> {
        Err(DeviceError)
    }

    fn block_count(&self) -> u64 {
        (self.len / BLOCK_SIZE) as u64
    }
}
