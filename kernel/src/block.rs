/// Unique identifier for a block device, used as page cache key.
pub type DeviceId = u32;

/// Block-oriented storage device interface.
///
/// All I/O is in whole 4KB blocks. No byte-level addressing — that's the
/// filesystem's job. The page cache sits between the filesystem and this trait.
pub trait BlockDevice: Send {
    fn device_id(&self) -> DeviceId;
    fn block_count(&self) -> u64;

    /// Read `count` contiguous blocks starting at `lba` into `buf`.
    /// `buf.len()` must equal `count as usize * block_size() as usize`.
    fn read_blocks(&mut self, lba: u64, count: u32, buf: &mut [u8]);

    /// Write `count` contiguous blocks starting at `lba` from `buf`.
    /// `buf.len()` must equal `count as usize * block_size() as usize`.
    fn write_blocks(&mut self, lba: u64, count: u32, buf: &[u8]);

    /// Flush any hardware write caches to persistent storage.
    fn flush(&mut self);
}
