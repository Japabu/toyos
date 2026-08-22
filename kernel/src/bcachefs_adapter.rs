use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use hashbrown::HashMap;

use bcachefs::{BlockIO, BlockBuf, BlockNum, DeviceError, FsError, Mounted, ReadWrite, ReadOnly, Formatted, SliceBlockIO, Extent};
use crate::file_backing::{FileBacking, FileBlocks, NvmeBacking, InitrdBacking};
use crate::file_cache::{self, FileId, Residency};
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

/// What an `FsError` means to the [`FileSystem`] trait's caller.
///
/// Exhaustive, so a variant added to `bcachefs` stops this compiling rather
/// than joining a catch-all. Every corruption variant is [`SyscallError::Io`]
/// and not `NotFound`: a btree node that does not decode is a volume that
/// cannot answer the question, which is the same thing to a caller as a device
/// that refused the transfer, and the opposite of a name that is not there.
fn as_syscall_error(err: &FsError) -> SyscallError {
    match err {
        FsError::NotFound => SyscallError::NotFound,
        FsError::NoSpace { .. } | FsError::EntryTooLarge { .. } => SyscallError::ResourceExhausted,
        FsError::NameTooLong { .. } => SyscallError::InvalidArgument,
        FsError::DeviceRead(_) | FsError::DeviceWrite(_) | FsError::DeviceSync => SyscallError::Io,
        FsError::BadMagic { .. }
        | FsError::UnsupportedVersion(_)
        | FsError::ChecksumMismatch { .. }
        | FsError::CorruptedKey(_)
        | FsError::CorruptedNode(_)
        | FsError::BlockOffDevice { .. }
        | FsError::TreeTooDeep(_)
        | FsError::BadSuperblock { .. }
        | FsError::NodeOverfull { .. } => SyscallError::Io,
    }
}

/// Log what went wrong and hand the caller the code for it.
///
/// The `FsError` carries a block number and a field name; `SyscallError` has
/// room for neither, and a triage reads the log. This used to answer `None`
/// instead, which put the sentinel `bcachefs::BlockIO` had just shed one layer
/// further up: a device that would not read looked to every caller exactly like
/// a file that was not there.
fn mapped<T>(op: &str, name: &str, result: Result<T, FsError>) -> Result<T, SyscallError> {
    result.map_err(|err| {
        log!("bcachefs: {} of '{}' failed: {:?}", op, name, err);
        as_syscall_error(&err)
    })
}

/// The same, for a `bcachefs` call whose `Ok(None)` means the name is absent.
fn present<T>(op: &str, name: &str, result: Result<Option<T>, FsError>) -> Result<T, SyscallError> {
    mapped(op, name, result)?.ok_or(SyscallError::NotFound)
}

/// Per-open-file cached resolution state.
struct OpenFileInfo {
    name: String,
    blocks: Arc<FileBlocks>,
}

/// VFS adapter for read-write bcachefs on NVMe.
pub struct BcacheFsAdapter {
    fs: Mounted<PageCacheBlockIO, ReadWrite>,
    open_files: HashMap<FileId, OpenFileInfo>,
    name_to_id: HashMap<String, FileId>,
    /// The one [`FileBlocks`] every backing for a name shares.
    ///
    /// Keyed by name and not by `FileId` because `open_backing` hands out a
    /// backing without opening a file at all — that is the one a spawned
    /// program's text lives behind, and it outlives every handle. `Weak` so the
    /// entry costs nothing once the last backing is dropped.
    blocks: HashMap<String, Weak<FileBlocks>>,
}

impl BcacheFsAdapter {
    pub fn new(fs: Mounted<PageCacheBlockIO, ReadWrite>) -> Self {
        Self {
            fs,
            open_files: HashMap::new(),
            name_to_id: HashMap::new(),
            blocks: HashMap::new(),
        }
    }

    /// The cell every backing for `name` reads through, made from `extents` if
    /// this is the first one.
    fn blocks_for(&mut self, name: &str, extents: Vec<Extent>) -> Arc<FileBlocks> {
        // Names whose last backing has gone are swept here rather than on a
        // timer: the map is only ever grown by this call, so this is the one
        // place where dropping them costs nothing extra.
        self.blocks.retain(|_, weak| weak.strong_count() > 0);

        if let Some(live) = self.blocks.get(name).and_then(Weak::upgrade) {
            return live;
        }
        let blocks = FileBlocks::new(extents);
        self.blocks.insert(String::from(name), Arc::downgrade(&blocks));
        blocks
    }

    /// Give up every backing that reads `name`'s blocks.
    ///
    /// Called wherever the filesystem hands those blocks back to the
    /// allocator — an unlink, a truncating create, a rename over an existing
    /// name. The next file takes them, so a backing that still names them
    /// reads that file's data: an information disclosure through ordinary
    /// filesystem operations, with nothing crafted about it.
    fn revoke(&mut self, name: &str) {
        if let Some(blocks) = self.blocks.remove(name).as_ref().and_then(Weak::upgrade) {
            blocks.revoke();
        }
    }
}

impl FileSystem for BcacheFsAdapter {
    /// The limit is checked on the result rather than before the work.
    /// `bcachefs::Mounted::list` exposes no count and `btree::collect_all`
    /// under it materialises the whole entry set first, so this makes the
    /// refusal uniform without making the allocation bounded — that half is
    /// the `bcachefs` crate's, and is filed.
    fn list(&mut self, limit: usize) -> Result<Vec<(String, u64)>, SyscallError> {
        // An empty listing, which is what this used to return, is a lie a
        // caller cannot tell from an empty directory.
        let names = mapped("list", "/", self.fs.list())?;
        if names.len() > limit {
            return Err(SyscallError::ResourceExhausted);
        }
        Ok(names)
    }

    fn file_mtime(&mut self, name: &str) -> Result<u64, SyscallError> {
        present("file_mtime", name, self.fs.file_mtime(name))
    }

    fn read_link(&mut self, name: &str) -> Result<Option<String>, SyscallError> {
        mapped("read_link", name, self.fs.read_link(name))
    }

    fn open_file(&mut self, name: &str) -> Result<(FileId, Option<Arc<dyn FileBacking>>), SyscallError> {
        if let Some(&file_id) = self.name_to_id.get(name) {
            file_cache::open(file_id);
            let info = self.open_files.get(&file_id).ok_or(SyscallError::NotFound)?;
            let backing = Arc::new(NvmeBacking::new(
                Arc::clone(&info.blocks),
                file_cache::size(file_id),
            ));
            return Ok((file_id, Some(backing)));
        }

        let (extents, size) = present("open", name, self.fs.file_extents(name))?;
        let blocks = self.blocks_for(name, extents);
        let file_id = file_cache::create_file(true); // evictable
        file_cache::set_size(file_id, size);

        self.name_to_id.insert(String::from(name), file_id);
        self.open_files.insert(file_id, OpenFileInfo {
            name: String::from(name),
            blocks: Arc::clone(&blocks),
        });

        Ok((file_id, Some(Arc::new(NvmeBacking::new(blocks, size)))))
    }

    fn create(&mut self, name: &str, mtime: u64) -> Result<FileId, SyscallError> {
        if let Some(&file_id) = self.name_to_id.get(name) {
            return Ok(file_id);
        }

        // `Mounted::create` frees whatever answered to this name — the blocks
        // of a program that is running out of it, if that is what it was.
        self.revoke(name);
        mapped("create", name, self.fs.create(name, &[], mtime))?;

        let file_id = file_cache::create_file(true);
        self.name_to_id.insert(String::from(name), file_id);
        let blocks = self.blocks_for(name, Vec::new());
        self.open_files.insert(file_id, OpenFileInfo {
            name: String::from(name),
            blocks,
        });
        Ok(file_id)
    }

    fn close_file(&mut self, file_id: FileId) {
        if let Some(info) = self.open_files.remove(&file_id) {
            self.name_to_id.remove(&info.name);
        }
    }

    fn delete(&mut self, name: &str) -> Result<(), SyscallError> {
        if let Some(&file_id) = self.name_to_id.get(name) {
            if file_cache::mark_deleted(file_id) == Residency::Gone {
                self.open_files.remove(&file_id);
            }
            self.name_to_id.remove(name);
        }
        self.revoke(name);
        if mapped("delete", name, self.fs.delete(name))? {
            Ok(())
        } else {
            Err(SyscallError::NotFound)
        }
    }

    fn rename(&mut self, old: &str, new: &str) -> Result<(), SyscallError> {
        if let Some(&target_id) = self.name_to_id.get(new) {
            if file_cache::mark_deleted(target_id) == Residency::Gone {
                self.open_files.remove(&target_id);
            }
            self.name_to_id.remove(new);
        }
        // The destination's blocks are freed by the rename; the source's are
        // carried over to the new name, so only the destination is revoked.
        self.revoke(new);

        mapped("rename", old, self.fs.rename(old, new))?;

        // Update name_to_id: source's FileId now lives under new name
        if let Some(file_id) = self.name_to_id.remove(old) {
            self.name_to_id.insert(String::from(new), file_id);
            if let Some(info) = self.open_files.get_mut(&file_id) {
                info.name = String::from(new);
            }
        }
        if let Some(blocks) = self.blocks.remove(old) {
            self.blocks.insert(String::from(new), blocks);
        }

        Ok(())
    }

    fn write_page(&mut self, file_id: FileId, page_idx: u32, data: &[u8; 4096]) -> Result<(), SyscallError> {
        let info = self.open_files.get(&file_id).ok_or(SyscallError::NotFound)?;
        let name = info.name.clone();
        let blocks = Arc::clone(&info.blocks);
        let block = blocks
            .with(|extents| self.fs.resolve_or_alloc_block(extents, page_idx))
            .ok_or(SyscallError::NotFound)?;
        let block = mapped("block allocation", &name, block)?;
        page_cache::raw_block_write(block, data).map_err(|_| {
            log!("bcachefs: write of block {block} for '{name}' failed");
            SyscallError::Io
        })
    }

    fn update_metadata(&mut self, file_id: FileId, size: u64, mtime: u64) -> Result<(), SyscallError> {
        let info = self.open_files.get(&file_id).ok_or(SyscallError::NotFound)?;
        let name = info.name.clone();
        let extents = Arc::clone(&info.blocks)
            .with(|extents| extents.clone())
            .ok_or(SyscallError::NotFound)?;
        mapped("update_metadata", &name, self.fs.update_metadata(&name, &extents, size, mtime))
    }

    fn create_symlink(&mut self, name: &str, target: &str) -> Result<(), SyscallError> {
        // As `create`: the symlink displaces whatever answered to this name
        // and the displaced entry's blocks go back to the allocator.
        self.revoke(name);
        mapped("create_symlink", name, self.fs.create_symlink(name, target))
    }

    fn sync(&mut self) -> Result<(), SyscallError> {
        mapped("sync", "/", self.fs.sync())
    }

    fn open_backing(&mut self, name: &str) -> Result<Arc<dyn FileBacking>, SyscallError> {
        let (extents, size) = present("open_backing", name, self.fs.file_extents(name))?;
        let blocks = self.blocks_for(name, extents);
        Ok(Arc::new(NvmeBacking::new(blocks, size)))
    }
}

/// VFS adapter for read-only bcachefs (initrd mounted in memory).
///
/// It holds the image rather than the image's base address, because an
/// [`InitrdBacking`] handed only a base can compute an address for any block
/// the btree names and has nothing to compare it against. `SliceBlockIO` is
/// `Copy`, so the mount and every backing check against the same length.
///
/// The `unsafe impl Send` this used to carry went with the raw pointer: a
/// `SliceBlockIO` is `Send + Sync` in its own crate, on its own argument.
pub struct ReadOnlyBcacheFsAdapter {
    fs: Mounted<SliceBlockIO, ReadOnly>,
    image: SliceBlockIO,
    name_to_id: HashMap<String, FileId>,
}

impl ReadOnlyBcacheFsAdapter {
    pub fn new(fs: Mounted<SliceBlockIO, ReadOnly>, image: SliceBlockIO) -> Self {
        Self { fs, image, name_to_id: HashMap::new() }
    }
}

impl FileSystem for ReadOnlyBcacheFsAdapter {
    /// The limit is checked on the result rather than before the work.
    /// `bcachefs::Mounted::list` exposes no count and `btree::collect_all`
    /// under it materialises the whole entry set first, so this makes the
    /// refusal uniform without making the allocation bounded — that half is
    /// the `bcachefs` crate's, and is filed.
    fn list(&mut self, limit: usize) -> Result<Vec<(String, u64)>, SyscallError> {
        let names = mapped("list", "/", self.fs.list())?;
        if names.len() > limit {
            return Err(SyscallError::ResourceExhausted);
        }
        Ok(names)
    }

    fn file_mtime(&mut self, name: &str) -> Result<u64, SyscallError> {
        present("file_mtime", name, self.fs.file_mtime(name))
    }

    fn read_link(&mut self, name: &str) -> Result<Option<String>, SyscallError> {
        mapped("read_link", name, self.fs.read_link(name))
    }

    fn open_file(&mut self, name: &str) -> Result<(FileId, Option<Arc<dyn FileBacking>>), SyscallError> {
        let (extents, size) = present("open", name, self.fs.file_extents(name))?;
        if let Some(&file_id) = self.name_to_id.get(name) {
            file_cache::open(file_id);
            let backing = Arc::new(InitrdBacking::new(self.image, extents, size));
            return Ok((file_id, Some(backing)));
        }

        let file_id = file_cache::create_file(true);
        file_cache::set_size(file_id, size);

        self.name_to_id.insert(String::from(name), file_id);

        let backing = Arc::new(InitrdBacking::new(self.image, extents, size));
        Ok((file_id, Some(backing)))
    }

    fn create(&mut self, _name: &str, _mtime: u64) -> Result<FileId, SyscallError> {
        Err(SyscallError::PermissionDenied)
    }

    fn close_file(&mut self, file_id: FileId) {
        let name = self.name_to_id.iter()
            .find(|(_, &v)| v == file_id)
            .map(|(k, _)| k.clone());
        if let Some(name) = name {
            self.name_to_id.remove(&name);
        }
    }

    /// Every write path answers `PermissionDenied`, which is what the mount
    /// *is* — the initrd is a read-only image and nothing on it can change.
    /// Distinct from `Io` on purpose: a caller retrying a refused write is
    /// right about a device and wrong about this.
    fn delete(&mut self, _name: &str) -> Result<(), SyscallError> {
        Err(SyscallError::PermissionDenied)
    }

    fn rename(&mut self, _old: &str, _new: &str) -> Result<(), SyscallError> {
        Err(SyscallError::PermissionDenied)
    }

    fn write_page(&mut self, _file_id: FileId, _page_idx: u32, _data: &[u8; 4096]) -> Result<(), SyscallError> {
        Err(SyscallError::PermissionDenied)
    }

    fn update_metadata(&mut self, _file_id: FileId, _size: u64, _mtime: u64) -> Result<(), SyscallError> {
        Err(SyscallError::PermissionDenied)
    }

    fn create_symlink(&mut self, _name: &str, _target: &str) -> Result<(), SyscallError> {
        Err(SyscallError::PermissionDenied)
    }

    fn sync(&mut self) -> Result<(), SyscallError> {
        Ok(())
    }

    fn open_backing(&mut self, name: &str) -> Result<Arc<dyn FileBacking>, SyscallError> {
        let (extents, size) = present("open_backing", name, self.fs.file_extents(name))?;
        Ok(Arc::new(InitrdBacking::new(self.image, extents, size)))
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

/// Mount a read-only bcachefs filesystem from an image already in memory
/// (initrd).
///
/// **It takes the image and not `(ptr, len)`, and that is the whole of what
/// this function used to get wrong about its own signature.** A raw pointer and
/// a length handed to something that reads through them is a call that ought to
/// be `unsafe` — `elf::read_backing_into` and
/// `elf::index::RelocationIndex::apply_to_page` were the last two of that shape
/// in this kernel, and both take a `mm::KernelSlice` now, which carries the
/// length the allocation gave it. Taking a `SliceBlockIO` moves the claim to
/// `SliceBlockIO::new`, which is already an `unsafe fn` and already states it,
/// so there is one claim about the initrd region in the whole kernel and it is
/// made where the region is named.
pub fn mount_initrd(image: SliceBlockIO) -> Mounted<SliceBlockIO, ReadOnly> {
    Mounted::<SliceBlockIO, ReadOnly>::open(image).expect("Failed to mount bcachefs initrd")
}
