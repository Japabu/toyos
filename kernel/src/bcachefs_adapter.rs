use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use hashbrown::HashMap;

use bcachefs::{BlockIO, BlockBuf, BlockNum, Mounted, ReadWrite, ReadOnly, Formatted, SliceBlockIO, Extent};
use crate::file_backing::{FileBacking, NvmeBacking, InitrdBacking};
use crate::file_cache::{self, FileId};
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

/// Per-open-file cached resolution state.
struct OpenFileInfo {
    name: String,
    extents: Vec<Extent>,
}

/// VFS adapter for read-write bcachefs on NVMe.
pub struct BcacheFsAdapter {
    fs: Mounted<PageCacheBlockIO, ReadWrite>,
    open_files: HashMap<FileId, OpenFileInfo>,
    name_to_id: HashMap<String, FileId>,
}

impl BcacheFsAdapter {
    pub fn new(fs: Mounted<PageCacheBlockIO, ReadWrite>) -> Self {
        Self { fs, open_files: HashMap::new(), name_to_id: HashMap::new() }
    }
}

impl FileSystem for BcacheFsAdapter {
    fn list(&mut self) -> Vec<(String, u64)> {
        self.fs.list().unwrap_or_default()
    }

    fn file_size(&mut self, name: &str) -> Option<u64> {
        self.fs.file_size_meta(name)
    }

    fn file_mtime(&mut self, name: &str) -> u64 {
        self.fs.file_mtime(name)
    }

    fn read_link(&mut self, name: &str) -> Option<String> {
        self.fs.read_link(name)
    }

    fn open_file(&mut self, name: &str) -> Option<(FileId, Option<Arc<dyn FileBacking>>)> {
        // Same file → same FileId
        if let Some(&file_id) = self.name_to_id.get(name) {
            file_cache::open(file_id);
            // Return existing backing
            let info = self.open_files.get(&file_id)?;
            let backing = Arc::new(NvmeBacking::new(info.extents.clone(), file_cache::size(file_id)));
            return Some((file_id, Some(backing)));
        }

        let (extents, size) = self.fs.file_extents(name)?;
        let file_id = file_cache::create_file(true); // evictable
        file_cache::set_size(file_id, size);

        self.name_to_id.insert(String::from(name), file_id);
        self.open_files.insert(file_id, OpenFileInfo {
            name: String::from(name),
            extents: extents.clone(),
        });

        let backing = Arc::new(NvmeBacking::new(extents, size));
        Some((file_id, Some(backing)))
    }

    fn create(&mut self, name: &str, mtime: u64) -> Result<FileId, &'static str> {
        if let Some(&file_id) = self.name_to_id.get(name) {
            return Ok(file_id);
        }

        // Create empty file in bcachefs
        self.fs.create(name, &[], mtime).map_err(|_| "create failed")?;

        let file_id = file_cache::create_file(true);
        self.name_to_id.insert(String::from(name), file_id);
        self.open_files.insert(file_id, OpenFileInfo {
            name: String::from(name),
            extents: Vec::new(),
        });
        Ok(file_id)
    }

    fn close_file(&mut self, file_id: FileId) {
        if file_cache::ref_count(file_id) == 0 {
            if let Some(info) = self.open_files.remove(&file_id) {
                self.name_to_id.remove(&info.name);
            }
        }
    }

    fn delete(&mut self, name: &str) -> bool {
        // Handle FileId cleanup
        if let Some(&file_id) = self.name_to_id.get(name) {
            file_cache::mark_deleted(file_id);
            if file_cache::ref_count(file_id) == 0 {
                self.open_files.remove(&file_id);
            }
            self.name_to_id.remove(name);
        }
        self.fs.delete(name)
    }

    fn delete_prefix(&mut self, prefix: &str) {
        // Collect FileIds to clean up
        let to_delete: Vec<String> = self.name_to_id.keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect();
        for name in &to_delete {
            if let Some(&file_id) = self.name_to_id.get(name.as_str()) {
                file_cache::mark_deleted(file_id);
                if file_cache::ref_count(file_id) == 0 {
                    self.open_files.remove(&file_id);
                }
            }
            self.name_to_id.remove(name.as_str());
        }
        self.fs.delete_prefix(prefix);
    }

    fn rename(&mut self, old: &str, new: &str) -> Result<(), &'static str> {
        // Handle target's FileId if it exists
        if let Some(&target_id) = self.name_to_id.get(new) {
            file_cache::mark_deleted(target_id);
            if file_cache::ref_count(target_id) == 0 {
                self.open_files.remove(&target_id);
            }
            self.name_to_id.remove(new);
        }

        // Delegate to bcachefs
        self.fs.rename(old, new).map_err(|_| "rename failed")?;

        // Update name_to_id: source's FileId now lives under new name
        if let Some(file_id) = self.name_to_id.remove(old) {
            self.name_to_id.insert(String::from(new), file_id);
            if let Some(info) = self.open_files.get_mut(&file_id) {
                info.name = String::from(new);
            }
        }

        Ok(())
    }

    fn write_page(&mut self, file_id: FileId, page_idx: u32, data: &[u8; 4096]) -> Result<(), &'static str> {
        let info = self.open_files.get_mut(&file_id).ok_or("file not open")?;
        let block = self.fs.resolve_or_alloc_block(&mut info.extents, page_idx)
            .map_err(|_| "block allocation failed")?;
        page_cache::raw_block_write(block, data);
        Ok(())
    }

    fn update_metadata(&mut self, file_id: FileId, size: u64, mtime: u64) -> Result<(), &'static str> {
        let info = self.open_files.get(&file_id).ok_or("file not open")?;
        self.fs.update_metadata(&info.name, &info.extents, size, mtime)
            .map_err(|_| "metadata update failed")
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
    name_to_id: HashMap<String, FileId>,
}

// Safety: initrd memory is static for the kernel's lifetime
unsafe impl Send for ReadOnlyBcacheFsAdapter {}

impl ReadOnlyBcacheFsAdapter {
    pub fn new(fs: Mounted<SliceBlockIO, ReadOnly>, initrd_base: *const u8) -> Self {
        Self { fs, initrd_base, name_to_id: HashMap::new() }
    }
}

impl FileSystem for ReadOnlyBcacheFsAdapter {
    fn list(&mut self) -> Vec<(String, u64)> {
        self.fs.list().unwrap_or_default()
    }

    fn file_size(&mut self, name: &str) -> Option<u64> {
        self.fs.file_size_meta(name)
    }

    fn file_mtime(&mut self, name: &str) -> u64 {
        self.fs.file_mtime(name)
    }

    fn read_link(&mut self, name: &str) -> Option<String> {
        self.fs.read_link(name)
    }

    fn open_file(&mut self, name: &str) -> Option<(FileId, Option<Arc<dyn FileBacking>>)> {
        // Same file → same FileId
        if let Some(&file_id) = self.name_to_id.get(name) {
            file_cache::open(file_id);
            let (extents, size) = self.fs.file_extents(name)?;
            let backing = Arc::new(InitrdBacking::new(self.initrd_base, extents, size));
            return Some((file_id, Some(backing)));
        }

        let (extents, size) = self.fs.file_extents(name)?;
        let file_id = file_cache::create_file(true);
        file_cache::set_size(file_id, size);

        self.name_to_id.insert(String::from(name), file_id);

        let backing = Arc::new(InitrdBacking::new(self.initrd_base, extents, size));
        Some((file_id, Some(backing)))
    }

    fn create(&mut self, _name: &str, _mtime: u64) -> Result<FileId, &'static str> {
        Err("read-only filesystem")
    }

    fn close_file(&mut self, file_id: FileId) {
        if file_cache::ref_count(file_id) == 0 {
            // Find and remove from name_to_id
            let name = self.name_to_id.iter()
                .find(|(_, &v)| v == file_id)
                .map(|(k, _)| k.clone());
            if let Some(name) = name {
                self.name_to_id.remove(&name);
            }
        }
    }

    fn delete(&mut self, _name: &str) -> bool { false }
    fn delete_prefix(&mut self, _prefix: &str) {}

    fn rename(&mut self, _old: &str, _new: &str) -> Result<(), &'static str> {
        Err("read-only filesystem")
    }

    fn write_page(&mut self, _file_id: FileId, _page_idx: u32, _data: &[u8; 4096]) -> Result<(), &'static str> {
        Err("read-only filesystem")
    }

    fn update_metadata(&mut self, _file_id: FileId, _size: u64, _mtime: u64) -> Result<(), &'static str> {
        Err("read-only filesystem")
    }

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
