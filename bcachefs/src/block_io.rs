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

/// The byte range block `block` occupies in an image, or `None` when that
/// range cannot be expressed at all.
///
/// **`checked_mul`, and that is not a spelling choice.** A block number reaches
/// this crate out of a btree that lives in the image it indexes, so a corrupt
/// or hostile image names any `u64` it likes — and `0x0020_0000_0000_0000 * 4096`
/// wraps. Under the kernel's build profile (`overflow-checks = true`) a wrapping
/// multiply is a *panic in the kernel* from input that crossed a trust boundary,
/// and under a release profile it is a small in-range offset into somebody
/// else's memory. Both are refusals here.
///
/// One function rather than the same three lines at each of the four call
/// sites: the bound every image-backed reader applies is this one.
fn byte_range(block: BlockNum) -> Option<(usize, usize)> {
    let off = (block.raw() as usize).checked_mul(BLOCK_SIZE)?;
    let end = off.checked_add(BLOCK_SIZE)?;
    Some((off, end))
}

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
        let (off, end) = byte_range(block).ok_or(DeviceError)?;
        buf.0.copy_from_slice(data.get(off..end).ok_or(DeviceError)?);
        Ok(())
    }

    fn write_block(&self, block: BlockNum, buf: &BlockBuf) -> Result<(), DeviceError> {
        let mut data = self.data.borrow_mut();
        let (off, end) = byte_range(block).ok_or(DeviceError)?;
        data.get_mut(off..end).ok_or(DeviceError)?.copy_from_slice(&buf.0);
        Ok(())
    }

    fn block_count(&self) -> u64 {
        (self.data.borrow().len() / BLOCK_SIZE) as u64
    }
}

/// Read-only block device backed by a static byte slice. Used for initrd in the kernel.
///
/// `Copy`, because the image is an address and a length and nothing else: a
/// mount and every file backing that reads out of the same image hold the same
/// pair, and the bound they check against is therefore the same one rather than
/// a copy somebody re-derived.
#[derive(Clone, Copy)]
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

    /// The `BLOCK_SIZE` bytes block `block` occupies, or `None` when this image
    /// does not reach that far.
    ///
    /// **The image is what bounds a block, and this is the only thing that
    /// knows both.** A reader that holds the image's extents but not its length
    /// — the kernel's initrd file backing was one — can compute an address for
    /// any block the btree names and has nothing to compare it against, so a
    /// corrupt or hostile image reads whatever was placed after it. The
    /// comparison belongs to whoever owns the length, which is this type.
    ///
    /// Zero-copy on purpose: the caller that wants the bytes somewhere else has
    /// [`BlockIO::read_block`], and the caller that is about to copy `n <=
    /// BLOCK_SIZE` of them into a page (demand paging) would otherwise pay a
    /// whole extra 4 KiB `memcpy` per faulted page for the bound.
    pub fn block(&self, block: BlockNum) -> Option<&[u8]> {
        let (off, end) = byte_range(block)?;
        self.as_slice().get(off..end)
    }
}

impl BlockIO for SliceBlockIO {
    fn read_block(&self, block: BlockNum, buf: &mut BlockBuf) -> Result<(), DeviceError> {
        buf.0.copy_from_slice(self.block(block).ok_or(DeviceError)?);
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

/// The bound an image-backed reader applies to a block number the image itself
/// named, at every way of getting it wrong.
///
/// It runs here rather than in a guest because there is nothing architectural
/// about it: a block number, a length, and the comparison between them. The
/// kernel's initrd backing had no length to compare against at all until
/// [`SliceBlockIO::block`] existed, which is what these tests are the control
/// for.
#[cfg(test)]
mod slice_bounds {
    use super::*;

    /// Three blocks of image, each byte stamped with its block number.
    fn image() -> Vec<u8> {
        let mut v = vec![0u8; 3 * BLOCK_SIZE];
        for b in 0..3 {
            v[b * BLOCK_SIZE..(b + 1) * BLOCK_SIZE].fill(b as u8 + 1);
        }
        v
    }

    fn io(data: &[u8]) -> SliceBlockIO {
        // SAFETY: `data` outlives every use below, and `len` is its own.
        unsafe { SliceBlockIO::new(data.as_ptr(), data.len()) }
    }

    #[test]
    fn every_block_the_image_holds_is_served_whole() {
        let data = image();
        let io = io(&data);
        assert_eq!(io.block_count(), 3);
        for b in 0..3u64 {
            let got = io.block(BlockNum::new(b)).expect("block inside the image");
            assert_eq!(got.len(), BLOCK_SIZE);
            assert!(got.iter().all(|&x| x == b as u8 + 1), "block {b} served the wrong bytes");
        }
    }

    /// The defect this exists for: the extent list lives *inside* the image, so
    /// a corrupt one names a block past its own end and the reader that has no
    /// length reads whatever the bootloader placed after it.
    #[test]
    fn a_block_past_the_end_is_refused_rather_than_read() {
        let data = image();
        let io = io(&data);
        assert!(io.block(BlockNum::new(3)).is_none(), "one block past the end");
        assert!(io.block(BlockNum::new(4)).is_none());
        assert!(io.block(BlockNum::new(u64::MAX / BLOCK_SIZE as u64)).is_none());
        let mut buf = BlockBuf::zeroed();
        assert_eq!(io.read_block(BlockNum::new(3), &mut buf), Err(DeviceError));
    }

    /// A block that *starts* inside the image and ends past it. `get(off..end)`
    /// is what makes this a refusal; an `off < len` test would serve a short
    /// read out of whatever follows.
    #[test]
    fn a_block_that_starts_inside_and_ends_outside_is_refused() {
        let data = vec![7u8; 2 * BLOCK_SIZE + 1];
        let io = io(&data);
        assert!(io.block(BlockNum::new(1)).is_some(), "the last whole block");
        assert!(io.block(BlockNum::new(2)).is_none(), "one byte of a block is not a block");
    }

    /// `block.raw() as usize * BLOCK_SIZE` wraps for a block number above
    /// `usize::MAX / 4096`. The kernel builds this crate with
    /// `overflow-checks = true`, so the unchecked multiply is a kernel panic
    /// from a number an image chose — and without the checks it is a small
    /// in-range offset into somebody else's memory.
    #[test]
    fn a_block_number_whose_byte_offset_wraps_is_refused_and_does_not_panic() {
        let data = image();
        let io = io(&data);
        for n in [
            usize::MAX as u64 / BLOCK_SIZE as u64 + 1,
            u64::MAX,
            1u64 << 52,
            (1u64 << 52) + 1,
        ] {
            assert!(io.block(BlockNum::new(n)).is_none(), "block {n:#x} was not refused");
            assert_eq!(byte_range(BlockNum::new(n)), None, "block {n:#x} named a byte range");
        }
        // The largest block number that does express a range still names one
        // no image holds.
        let last = usize::MAX as u64 / BLOCK_SIZE as u64 - 1;
        assert!(byte_range(BlockNum::new(last)).is_some());
        assert!(io.block(BlockNum::new(last)).is_none());
    }

    /// An empty image holds no block, including block 0 — the superblock's.
    #[test]
    fn an_empty_image_holds_no_block_at_all() {
        let data: Vec<u8> = Vec::new();
        let io = io(&data);
        assert_eq!(io.block_count(), 0);
        assert!(io.block(BlockNum::new(0)).is_none());
    }
}
