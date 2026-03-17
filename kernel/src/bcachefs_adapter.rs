use alloc::borrow::Cow;
use alloc::string::String;
use alloc::vec::Vec;

use bcachefs::{BlockIO, BlockBuf, BlockNum, Mounted, ReadWrite, Formatted};
use crate::page_cache;
use crate::vfs::FileSystem;

/// BlockIO implementation that wraps the kernel's global PageCache.
/// Each call acquires the page_cache lock, performs the operation, and releases it.
pub struct PageCacheBlockIO;

impl BlockIO for PageCacheBlockIO {
    fn read_block(&self, block: BlockNum, buf: &mut BlockBuf) {
        let mut guard = page_cache::lock();
        let (cache, dev) = guard.cache_and_dev();
        let page = cache.read(dev, block.raw());
        buf.as_bytes_mut().copy_from_slice(page);
    }

    fn write_block(&self, block: BlockNum, buf: &BlockBuf) {
        let mut guard = page_cache::lock();
        let (cache, dev) = guard.cache_and_dev();
        let page = cache.write_new(dev, block.raw());
        page.copy_from_slice(buf.as_bytes());
    }

    fn block_count(&self) -> u64 {
        let guard = page_cache::lock();
        let inner = guard;
        // Access block_count through the cache's stored value
        inner.block_count()
    }

    fn sync(&self) {
        let mut guard = page_cache::lock();
        let (cache, dev) = guard.cache_and_dev();
        cache.sync(dev);
    }
}

/// VFS FileSystem adapter for bcachefs on NVMe (read-write).
pub struct BcacheFsAdapter {
    fs: Mounted<PageCacheBlockIO, ReadWrite>,
}

impl BcacheFsAdapter {
    pub fn new(fs: Mounted<PageCacheBlockIO, ReadWrite>) -> Self {
        Self { fs }
    }
}

impl FileSystem for BcacheFsAdapter {
    fn list(&mut self) -> Vec<(String, u64)> {
        self.fs.list().unwrap_or_default()
    }

    fn read_file(&mut self, name: &str) -> Result<Cow<'static, [u8]>, &'static str> {
        self.fs.read_file(name)
            .map(Cow::Owned)
            .map_err(|_| "not found")
    }

    fn read_link(&mut self, name: &str) -> Option<String> {
        self.fs.read_link(name)
    }

    fn file_mtime(&mut self, name: &str) -> u64 {
        self.fs.file_mtime(name)
    }

    fn create(&mut self, name: &str, data: &[u8], mtime: u64) -> Result<(), &'static str> {
        self.fs.create(name, data, mtime).map_err(|_| "create failed")
    }

    fn delete(&mut self, name: &str) -> bool {
        self.fs.delete(name)
    }

    fn delete_prefix(&mut self, prefix: &str) {
        self.fs.delete_prefix(prefix);
    }

    fn create_symlink(&mut self, name: &str, target: &str) -> Result<(), &'static str> {
        self.fs.create_symlink(name, target).map_err(|_| "symlink failed")
    }

    fn sync(&mut self) {
        self.fs.sync();
    }

    fn file_block_map(&mut self, name: &str) -> Option<Vec<u64>> {
        self.fs.file_block_map(name)
    }
}

/// Format a new bcachefs filesystem on the NVMe device via PageCache.
pub fn format() -> Mounted<PageCacheBlockIO, ReadWrite> {
    let io = PageCacheBlockIO;
    let fs = Formatted::format(io);
    fs.mount()
}

/// Try to mount an existing bcachefs filesystem from NVMe.
pub fn mount() -> Option<Mounted<PageCacheBlockIO, ReadWrite>> {
    let io = PageCacheBlockIO;
    Mounted::<PageCacheBlockIO, ReadWrite>::open(io).ok()
}
