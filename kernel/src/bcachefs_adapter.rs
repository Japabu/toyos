use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use hashbrown::HashMap;

use bcachefs::{BlockIO, BlockBuf, BlockNum, DeviceError, FsError, Mounted, ReadWrite, ReadOnly, Formatted, SliceBlockIO, Extent};
use crate::file_backing::{FileBacking, NvmeBacking, InitrdBacking};
use crate::file_cache::{self, FileId};
use crate::page_cache;
use toyos_abi::syscall::SyscallError;

use crate::vfs::FileSystem;

/// BlockIO implementation that wraps the kernel's global PageCache.
pub struct PageCacheBlockIO;

/// The device error channel now runs the whole way: `BlockDevice` reports a
/// refused transfer, the page cache propagates it, and `bcachefs::BlockIO`
/// carries it into `FsError`. Nothing here invents a value.
///
/// This used to serve zeros and a log line, which was fail-closed rather than
/// correct — zeros fail bcachefs's structural checks, so a read error reached
/// the btree looking like corruption and a *write* error looked like a write.
impl BlockIO for PageCacheBlockIO {
    fn read_block(&self, block: BlockNum, buf: &mut BlockBuf) -> Result<(), DeviceError> {
        let mut guard = page_cache::lock();
        let (cache, dev) = guard.cache_and_dev();
        let page = cache.read(dev, block.raw()).map_err(|_| DeviceError)?;
        buf.as_bytes_mut().copy_from_slice(page);
        Ok(())
    }

    fn write_block(&self, block: BlockNum, buf: &BlockBuf) -> Result<(), DeviceError> {
        let mut guard = page_cache::lock();
        let (cache, dev) = guard.cache_and_dev();
        let page = cache.write_new(dev, block.raw()).map_err(|_| DeviceError)?;
        page.copy_from_slice(buf.as_bytes());
        Ok(())
    }

    fn block_count(&self) -> u64 {
        let guard = page_cache::lock();
        guard.block_count()
    }

    fn sync(&self) -> Result<(), DeviceError> {
        let mut guard = page_cache::lock();
        let (cache, dev) = guard.cache_and_dev();
        cache.sync(dev).map_err(|_| DeviceError)
    }
}

/// Log an `FsError` the [`FileSystem`] trait has no channel for, and answer
/// with the trait's "nothing here".
///
/// `file_size` and `read_link` return `Option`, `file_mtime` a bare `u64`,
/// `delete` a `bool`: every one of them reads a device failure as "no such
/// file". That is the same sentinel `bcachefs::BlockIO` used to be, moved one
/// layer up rather than removed — the trait is shared with `tmpfs` and
/// `fat32_adapter` and growing it a channel is its own change. Filed.
fn no_channel<T>(op: &str, name: &str, result: Result<T, FsError>) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(err) => {
            log!("bcachefs: {} of '{}' failed: {:?}", op, name, err);
            None
        }
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
    /// The limit is checked on the result rather than before the work.
    /// `bcachefs::Mounted::list` exposes no count and `btree::collect_all`
    /// under it materialises the whole entry set first, so this makes the
    /// refusal uniform without making the allocation bounded — that half is
    /// the `bcachefs` crate's, and is filed.
    fn list(&mut self, limit: usize) -> Result<Vec<(String, u64)>, SyscallError> {
        // `Unknown` because no `SyscallError` variant says "the device did not
        // answer" — the same gap `fd::try_write` hits, filed as one issue. An
        // empty listing, which is what this used to return, is a lie a caller
        // cannot tell from an empty directory.
        let names = self.fs.list().map_err(|err| {
            log!("bcachefs: list failed: {:?}", err);
            SyscallError::Unknown
        })?;
        if names.len() > limit {
            return Err(SyscallError::ResourceExhausted);
        }
        Ok(names)
    }

    fn file_size(&mut self, name: &str) -> Option<u64> {
        no_channel("file_size", name, self.fs.file_size_meta(name)).flatten()
    }

    fn file_mtime(&mut self, name: &str) -> u64 {
        no_channel("file_mtime", name, self.fs.file_mtime(name)).flatten().unwrap_or(0)
    }

    fn read_link(&mut self, name: &str) -> Option<String> {
        no_channel("read_link", name, self.fs.read_link(name)).flatten()
    }

    fn open_file(&mut self, name: &str) -> Option<(FileId, Option<Arc<dyn FileBacking>>)> {
        if let Some(&file_id) = self.name_to_id.get(name) {
            file_cache::open(file_id);
            let info = self.open_files.get(&file_id)?;
            let backing = Arc::new(NvmeBacking::new(info.extents.clone(), file_cache::size(file_id)));
            return Some((file_id, Some(backing)));
        }

        let (extents, size) = no_channel("open", name, self.fs.file_extents(name)).flatten()?;
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

        self.fs.create(name, &[], mtime).map_err(|err| {
            log!("bcachefs: create of '{}' failed: {:?}", name, err);
            "create failed"
        })?;

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
        if let Some(&file_id) = self.name_to_id.get(name) {
            file_cache::mark_deleted(file_id);
            if file_cache::ref_count(file_id) == 0 {
                self.open_files.remove(&file_id);
            }
            self.name_to_id.remove(name);
        }
        no_channel("delete", name, self.fs.delete(name)).unwrap_or(false)
    }

    fn delete_prefix(&mut self, prefix: &str) {
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
        no_channel("delete_prefix", prefix, self.fs.delete_prefix(prefix));
    }

    fn rename(&mut self, old: &str, new: &str) -> Result<(), &'static str> {
        if let Some(&target_id) = self.name_to_id.get(new) {
            file_cache::mark_deleted(target_id);
            if file_cache::ref_count(target_id) == 0 {
                self.open_files.remove(&target_id);
            }
            self.name_to_id.remove(new);
        }

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
        page_cache::raw_block_write(block, data).map_err(|_| "block write failed")?;
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

    fn sync(&mut self) -> Result<(), &'static str> {
        self.fs.sync().map_err(|err| {
            log!("bcachefs: sync failed: {:?}", err);
            "sync did not reach the device"
        })
    }

    fn open_backing(&mut self, name: &str) -> Option<Arc<dyn FileBacking>> {
        let (extents, size) = no_channel("open_backing", name, self.fs.file_extents(name)).flatten()?;
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
    /// The limit is checked on the result rather than before the work.
    /// `bcachefs::Mounted::list` exposes no count and `btree::collect_all`
    /// under it materialises the whole entry set first, so this makes the
    /// refusal uniform without making the allocation bounded — that half is
    /// the `bcachefs` crate's, and is filed.
    fn list(&mut self, limit: usize) -> Result<Vec<(String, u64)>, SyscallError> {
        let names = self.fs.list().map_err(|err| {
            log!("initrd: list failed: {:?}", err);
            SyscallError::Unknown
        })?;
        if names.len() > limit {
            return Err(SyscallError::ResourceExhausted);
        }
        Ok(names)
    }

    fn file_size(&mut self, name: &str) -> Option<u64> {
        no_channel("file_size", name, self.fs.file_size_meta(name)).flatten()
    }

    fn file_mtime(&mut self, name: &str) -> u64 {
        no_channel("file_mtime", name, self.fs.file_mtime(name)).flatten().unwrap_or(0)
    }

    fn read_link(&mut self, name: &str) -> Option<String> {
        no_channel("read_link", name, self.fs.read_link(name)).flatten()
    }

    fn open_file(&mut self, name: &str) -> Option<(FileId, Option<Arc<dyn FileBacking>>)> {
        let (extents, size) = no_channel("open", name, self.fs.file_extents(name)).flatten()?;
        if let Some(&file_id) = self.name_to_id.get(name) {
            file_cache::open(file_id);
            let backing = Arc::new(InitrdBacking::new(self.initrd_base, extents, size));
            return Some((file_id, Some(backing)));
        }

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

    fn sync(&mut self) -> Result<(), &'static str> {
        Ok(())
    }

    fn open_backing(&mut self, name: &str) -> Option<Arc<dyn FileBacking>> {
        let (extents, size) =
            no_channel("open_backing", name, self.fs.file_extents(name)).flatten()?;
        Some(Arc::new(InitrdBacking::new(self.initrd_base, extents, size)))
    }
}

/// Format a new bcachefs filesystem on the NVMe device via PageCache.
///
/// Destroys everything on the device. [`probe`] is the only caller that is
/// entitled to reach it, and only on [`Storage::Designated`].
fn format() -> Option<Mounted<PageCacheBlockIO, ReadWrite>> {
    match Formatted::format(PageCacheBlockIO) {
        Ok(fs) => Some(fs.mount()),
        Err(err) => {
            // The disk said we may destroy what is on it and then would not
            // take the new volume. `open_home` falls back to a tmpfs `/home`,
            // the same as for a disk that is not ours: a half-written volume
            // is not one to mount.
            log!("storage: formatting the designated device failed: {:?}", err);
            None
        }
    }
}

/// Try to mount an existing bcachefs filesystem from NVMe.
fn mount() -> Option<Mounted<PageCacheBlockIO, ReadWrite>> {
    let io = PageCacheBlockIO;
    Mounted::<PageCacheBlockIO, ReadWrite>::open(io).ok()
}

/// What the machine's block device is, as far as we are entitled to care.
///
/// The whole point of this enum is that there is no fourth arm and no default
/// that writes. `Foreign` is the state of every disk that has ever belonged to
/// anyone else, and it is also the state of a blank one — which is exactly why
/// it cannot be treated as permission.
pub enum Storage {
    /// A ToyOS volume, mounted read-write. Identified positively, by its own
    /// superblock, not by elimination.
    Ours(Mounted<PageCacheBlockIO, ReadWrite>),
    /// The device carries a designation stamp naming its own size: somebody
    /// deliberately said we may destroy what is here.
    Designated,
    /// Anything else. Never written to, under any circumstances.
    Foreign,
}

/// Decide what the device is, from one read of block 0.
///
/// **A failed mount is not consent.** It is the single most likely state of a
/// disk that belongs to someone else: an unformatted disk, a disk holding
/// another operating system, and a ToyOS volume too corrupt to open are all
/// indistinguishable from each other and all three arrive here as "mount
/// returned None". The kernel used to format on that, which meant the first
/// boot on any machine with a disk in it would take the disk. The only reason
/// the T14's first boot did not is that an unrelated panic in `page_cache::init`
/// happened to come first, and that panic has since been fixed — so the bug we
/// removed was the interlock.
///
/// One read decides all three because bcachefs puts its superblock at block 0
/// too, so a disk cannot be both ours and awaiting designation. Reading is
/// safe on any disk whatsoever; nothing below writes.
pub fn probe() -> Storage {
    if let Some(fs) = mount() {
        log!("storage: mounted the ToyOS volume at block 0");
        return Storage::Ours(fs);
    }
    if designated() {
        log!("storage: block 0 designates this device for ToyOS — formatting it");
        return Storage::Designated;
    }
    log!(
        "storage: no ToyOS volume and no designation stamp at block 0 — this disk is not \
         ours and nothing will be written to it"
    );
    Storage::Foreign
}

/// Whether block 0 carries a designation stamp for a device of *this* size.
///
/// The size is half the stamp and not decoration: without it, a designated
/// image copied or restored onto a different disk would designate that disk
/// too. With it, designation does not survive being moved.
fn designated() -> bool {
    let mut guard = page_cache::lock();
    let blocks = guard.block_count();
    let (cache, dev) = guard.cache_and_dev();
    // A disk whose block 0 cannot be read has not said this kernel may format
    // it, and a read error is the least convincing consent there is.
    let Ok(block0) = cache.read(dev, 0) else {
        log!("storage: block 0 could not be read; this disk is not ours to format");
        return false;
    };

    let magic = bcachefs::DESIGNATION_MAGIC;
    if block0.len() < bcachefs::DESIGNATION_BLOCKS_OFFSET + 8
        || block0[..magic.len()] != magic
    {
        return false;
    }
    let mut stamped = [0u8; 8];
    stamped.copy_from_slice(
        &block0[bcachefs::DESIGNATION_BLOCKS_OFFSET..bcachefs::DESIGNATION_BLOCKS_OFFSET + 8],
    );
    let stamped = u64::from_le_bytes(stamped);
    if stamped != blocks {
        log!(
            "storage: a designation stamp at block 0 names {} blocks, but this device has {} — \
             ignoring it",
            stamped, blocks
        );
        return false;
    }
    true
}

/// The `/home` filesystem, and the only path on which `format` runs.
///
/// `None` means the device is not ours: the caller mounts a tmpfs instead, so
/// a machine whose disk we may not touch still boots to a working system with
/// a volatile `/home` rather than panicking or, far worse, helping itself.
pub fn open_home() -> Option<Mounted<PageCacheBlockIO, ReadWrite>> {
    match probe() {
        Storage::Ours(fs) => Some(fs),
        Storage::Designated => format(),
        Storage::Foreign => None,
    }
}

/// Mount a read-only bcachefs filesystem from a memory slice (initrd).
pub fn mount_initrd(ptr: *const u8, len: usize) -> Mounted<SliceBlockIO, ReadOnly> {
    let io = unsafe { SliceBlockIO::new(ptr, len) };
    Mounted::<SliceBlockIO, ReadOnly>::open(io).expect("Failed to mount bcachefs initrd")
}
