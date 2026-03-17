use alloc::borrow::Cow;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use bcachefs::{BlockIO, BlockBuf, BlockNum, Mounted, ReadWrite, ReadOnly, Formatted, SliceBlockIO};
use crate::file_backing::{FileBacking, NvmeBacking, InitrdBacking};
use crate::page_cache;
use crate::vfs::FileSystem;

/// BlockIO implementation that wraps the kernel's global PageCache.
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
        guard.block_count()
    }

    fn sync(&self) {
        let mut guard = page_cache::lock();
        let (cache, dev) = guard.cache_and_dev();
        cache.sync(dev);
    }
}

/// VFS adapter for read-write bcachefs on NVMe.
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
        self.fs.read_file(name).map(Cow::Owned).map_err(|_| "not found")
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

    fn open_backing(&mut self, name: &str) -> Option<Arc<dyn FileBacking>> {
        let (extents, size) = self.fs.file_extents(name)?;
        Some(Arc::new(NvmeBacking::new(extents, size)))
    }
}

/// VFS adapter for read-only bcachefs (initrd mounted in memory).
pub struct ReadOnlyBcacheFsAdapter {
    fs: Mounted<SliceBlockIO, ReadOnly>,
    initrd_base: *const u8,
}

// Safety: initrd memory is static for the kernel's lifetime
unsafe impl Send for ReadOnlyBcacheFsAdapter {}

impl ReadOnlyBcacheFsAdapter {
    pub fn new(fs: Mounted<SliceBlockIO, ReadOnly>, initrd_base: *const u8) -> Self {
        Self { fs, initrd_base }
    }
}

impl FileSystem for ReadOnlyBcacheFsAdapter {
    fn list(&mut self) -> Vec<(String, u64)> {
        self.fs.list().unwrap_or_default()
    }

    fn read_file(&mut self, name: &str) -> Result<Cow<'static, [u8]>, &'static str> {
        self.fs.read_file(name).map(Cow::Owned).map_err(|_| "not found")
    }

    fn read_link(&mut self, name: &str) -> Option<String> {
        self.fs.read_link(name)
    }

    fn file_mtime(&mut self, name: &str) -> u64 {
        self.fs.file_mtime(name)
    }

    fn create(&mut self, _name: &str, _data: &[u8], _mtime: u64) -> Result<(), &'static str> {
        Err("read-only filesystem")
    }

    fn delete(&mut self, _name: &str) -> bool {
        false
    }

    fn delete_prefix(&mut self, _prefix: &str) {}

    fn create_symlink(&mut self, _name: &str, _target: &str) -> Result<(), &'static str> {
        Err("read-only filesystem")
    }

    fn sync(&mut self) {}

    fn open_backing(&mut self, name: &str) -> Option<Arc<dyn FileBacking>> {
        let (extents, size) = self.fs.file_extents(name)?;
        Some(Arc::new(InitrdBacking::new(self.initrd_base, extents, size)))
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

/// Mount a read-only bcachefs filesystem from a memory slice (initrd).
pub fn mount_initrd(ptr: *const u8, len: usize) -> Mounted<SliceBlockIO, ReadOnly> {
    let io = unsafe { SliceBlockIO::new(ptr, len) };
    Mounted::<SliceBlockIO, ReadOnly>::open(io).expect("Failed to mount bcachefs initrd")
}
