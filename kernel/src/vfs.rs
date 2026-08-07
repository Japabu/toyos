use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use hashbrown::HashMap;

use core::ops::{Deref, DerefMut};
use toyos_abi::syscall::SyscallError;
use crate::file_cache::FileId;
use crate::sync::{Lock, LockGuard};

static VFS: Lock<Option<Vfs>> = Lock::new(None);

pub fn init() {
    *VFS.lock() = Some(Vfs::new());
}

pub struct VfsGuard(LockGuard<'static, Option<Vfs>>);

impl Deref for VfsGuard {
    type Target = Vfs;
    fn deref(&self) -> &Vfs { self.0.as_ref().expect("VFS not initialized") }
}

impl DerefMut for VfsGuard {
    fn deref_mut(&mut self) -> &mut Vfs { self.0.as_mut().expect("VFS not initialized") }
}

pub fn lock() -> VfsGuard {
    VfsGuard(VFS.lock())
}

/// The VFS, or `None` if another CPU has it.
///
/// For a caller that must not wait on a filesystem — the idle loop, which is
/// about to run a scheduler pass — and for the one case where waiting would
/// not merely be slow: a thread that panicked while holding this lock never
/// releases it, and `Lock::lock` turns that into a second panic after 500M
/// spins. Known issues records that hazard; this is how a caller declines to
/// inherit it.
pub fn try_lock() -> Option<VfsGuard> {
    let guard = VFS.try_lock()?;
    if guard.is_none() {
        return None;
    }
    Some(VfsGuard(guard))
}

/// Trait abstracting filesystem operations so the VFS can hold
/// heterogeneous mount points (initrd on SliceDisk, nvme on NvmeDisk).
///
/// # Every answer is a `Result`, and the error is the ABI's
///
/// The device error channel runs from [`crate::block::BlockDevice`] up through
/// [`crate::file_backing::FileBacking`] and `bcachefs::BlockIO`, each fallible
/// so that nothing in the middle invents a value. This trait was where it
/// stopped: `open_file` and `read_link` returned `Option`, `file_mtime` a bare
/// `u64` and `delete` a `bool`, and every one of those read a device that would
/// not answer as *no such file*. That is not a degradation a caller can act on
/// — `fd::open` created an empty file over one that exists, because `CREATE`
/// acted on the same `None` a refused transfer produced.
///
/// [`SyscallError`] and not a filesystem error type of its own, because there
/// is no second consumer: the only thing above this trait is the syscall layer,
/// and a vocabulary translated once more on the way out is a vocabulary two
/// layers can come to disagree about. [`SyscallError::Io`] is the variant this
/// channel exists for; `NotFound` means the name is not there, and nothing
/// else may.
///
/// The detail belongs in the log rather than in the error. `BlockError` carries
/// nothing on purpose — which endpoint stalled and what the sense key was is in
/// the driver's own line — so an implementation here logs what it knows and
/// returns the code.
pub trait FileSystem: Send {
    /// Every name in this mount, or `ResourceExhausted` if there are more than
    /// `limit` of them.
    ///
    /// The limit is on the mount and not on the directory being listed, because
    /// that is what this call materialises: there is no per-directory index
    /// anywhere in the VFS, so every `readdir` builds the whole mount's listing
    /// and filters it. `limit` is [`MAX_LIST_ENTRIES`] at the only call sites.
    ///
    /// **An implementation must refuse before it allocates**, which is the only
    /// reason this takes a limit rather than the caller checking the length it
    /// gets back. `TmpFs` does; the two bcachefs adapters cannot, because
    /// `bcachefs::Mounted::list` has no count primitive and `btree::collect_all`
    /// under it builds the whole entry set first. Their check is on the result,
    /// so it makes the refusal uniform without making the allocation bounded —
    /// see known issues.
    fn list(&mut self, limit: usize) -> Result<Vec<(String, u64)>, SyscallError>;

    /// When `name` was last written, in whatever epoch the mount keeps.
    fn file_mtime(&mut self, name: &str) -> Result<u64, SyscallError>;

    /// What `name` points at, or `Ok(None)` if it is not a symbolic link.
    ///
    /// `Ok(None)` covers a name that is not there at all, and that is not a
    /// sentinel this signature could remove: both callers follow it with an
    /// `open_file` or an `open_backing` of the same name, which answers
    /// `NotFound` for the one and opens the other. What it must never fold in
    /// is a device that would not say, which is what the `Err` is for.
    fn read_link(&mut self, name: &str) -> Result<Option<String>, SyscallError>;

    /// Open a file. Returns (FileId, optional backing for cache misses).
    /// Must return the SAME FileId for the same file across multiple opens.
    fn open_file(&mut self, name: &str) -> Result<(FileId, Option<alloc::sync::Arc<dyn crate::file_backing::FileBacking>>), SyscallError>;
    /// Create an empty file. Returns FileId. Registers in name→FileId map.
    fn create(&mut self, name: &str, mtime: u64) -> Result<FileId, SyscallError>;
    /// Release filesystem-side state for a `FileId`.
    ///
    /// Reached only from a caller whose own `file_cache::release` returned the
    /// last reference, under the VFS lock that a reopen would need — so the
    /// answer arrives established, and an implementation asking the cache again
    /// would be asking a second question at a second moment.
    fn close_file(&mut self, file_id: FileId);

    /// Unlink `name`, or `NotFound` if there was nothing by that name.
    fn delete(&mut self, name: &str) -> Result<(), SyscallError>;
    fn rename(&mut self, old: &str, new: &str) -> Result<(), SyscallError>;

    /// Write a single dirty page to persistent storage. The filesystem resolves
    /// page_idx to a disk block (allocating if needed).
    fn write_page(&mut self, file_id: FileId, page_idx: u32, data: &[u8; 4096]) -> Result<(), SyscallError>;
    /// Update file metadata (size, mtime) after flushing dirty pages.
    fn update_metadata(&mut self, file_id: FileId, size: u64, mtime: u64) -> Result<(), SyscallError>;

    fn create_symlink(&mut self, name: &str, target: &str) -> Result<(), SyscallError>;

    /// Push everything this filesystem has buffered all the way to the device,
    /// the device's own write cache included.
    ///
    /// Fallible for the same reason [`crate::block::BlockDevice::read_blocks`]
    /// is: a sync whose failure the caller cannot see is indistinguishable from
    /// one that worked, and the caller here is a log that writes a line when it
    /// is told something went wrong — so swallowing the error made every failed
    /// sync produce the pending bytes that ask for the next one.
    fn sync(&mut self) -> Result<(), SyscallError>;

    /// Open a file backing for demand-paged ELF loading (separate from fd I/O).
    ///
    /// No default body. One answering "nothing here" would be the sentinel this
    /// trait exists to have removed, wearing a mount's own signature: a
    /// filesystem that forgot to implement it would report every program on it
    /// as missing.
    fn open_backing(&mut self, name: &str) -> Result<alloc::sync::Arc<dyn crate::file_backing::FileBacking>, SyscallError>;
}


/// Whether userland may modify a mount.
///
/// Stated at every `mount` call and defaulted nowhere, so a volume added later
/// cannot inherit the wrong answer by omission.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum UserAccess {
    ReadWrite,
    /// The kernel's own volume. Reachable and readable, and every syscall that
    /// would change it is refused — see [`Vfs::user_may_modify`].
    KernelOnly,
}

struct Mount {
    fs: Box<dyn FileSystem>,
    access: UserAccess,
}

/// Virtual filesystem that dispatches to named mount points.
pub struct Vfs {
    root: Option<Box<dyn FileSystem>>,
    mounts: HashMap<String, Mount>,
    created_dirs: hashbrown::HashSet<String>,
}

/// Longest absolute path the VFS will hand back, and so the longest a process's
/// `cwd` can ever be.
///
/// This exists because a bound can be defeated by composition. `MAX_USER_STR`
/// (64 KiB) really does bound every path *argument*, and its own derivation
/// says the number is set by the largest allocation derived from it. But
/// `resolve_absolute` prepends `cwd` before handing the result to `normalize`,
/// and `cwd` was bounded by nothing — so the input `MAX_USER_STR` was sized
/// against stopped being the input `normalize` actually saw. The check was
/// real; the assumption behind it had quietly stopped holding.
///
/// The number is derived, not picked. Let `L = MAX_PATH + 1 + MAX_USER_STR` be
/// the longest string reaching `normalize`. Its largest derived allocation is
/// the `Vec<&str>` of components: 16 bytes each, and a path of `"a/a/a/…"`
/// yields one component per two input bytes, so the vector holds up to
/// `ceil(L/2)` of them. `Vec` grows by doubling, so the buffer is
/// `next_pow2(ceil(L/2)) * 16` — and that single allocation must stay under
/// `mm::MAX_HEAP_ALLOC` (2_093_056), above which `KernelAllocator::alloc`
/// asserts.
///
/// At 4096: `L = 69_633`, `ceil(L/2) = 34_817`, `next_pow2 = 65_536`, so the
/// vector is 1 MiB — a factor of two under the ceiling. The joined `String` is
/// at most `L` bytes and never competes.
///
/// `MAX_USER_STR` dominates that sum, so this bound is a function of it: if
/// `MAX_USER_STR` ever rises, re-run the arithmetic above rather than assuming
/// this constant still holds. 64 KiB is already close to the cliff on its own —
/// `MAX_PATH = 65_535` would put `ceil(L/2)` at 65_537, one element past the
/// doubling step that lands on 2 MiB.
pub const MAX_PATH: usize = 4096;

/// The most entries one `FileSystem::list` may materialise.
///
/// The listing is a *derived* collection and `MAX_PATH` does not constrain it:
/// every name in it is individually short, and it is the count that grows. A
/// `read_dir` over 32,769 files in one tmpfs directory panicked the kernel —
/// measured, 1.8 s, from `fs::write` in a loop — which is the same shape as the
/// `cwd` accumulation `MAX_PATH` closed, one collection further out.
///
/// Derived, not picked. Three allocations scale with the entry count `N`, and
/// each must stay under `mm::MAX_HEAP_ALLOC` (2_093_056):
///
/// - the `Vec<(String, u64)>` `FileSystem::list` returns: `N * 32`, and the
///   32 is const-asserted below rather than believed.
/// - `Vfs::list`'s own `result`, same element: reserved *exactly* from a
///   counting pass, so it is `<= N * 32` with no growth-by-doubling overshoot.
///   That overshoot is what actually fired — `RawVec::grow_one` asking for the
///   *doubled* capacity, at half the entry count the element size suggests.
/// - `seen_dirs`, a `hashbrown::HashSet<String>` holding one entry per distinct
///   subdirectory name — worst case `N`, when every entry is `d<i>/f`.
///   hashbrown rounds to a power-of-two bucket count above `N * 8/7` and pays
///   24 bytes plus one control byte per bucket.
///
/// At 16_384 those are 524_288, at most 524_288, and `32_768 * 25 = 819_216`:
/// the worst is a factor of 2.5 under the ceiling, which is margin for a
/// hashbrown whose per-bucket cost changes rather than a number that has to be
/// re-derived when it does. Both worst cases are exercised at exactly this
/// count by `readdir_bound`, so the derivation is checked and not just written
/// down.
///
/// This bounds the *mount*, not the directory — see `FileSystem::list`.
pub const MAX_LIST_ENTRIES: usize = 16_384;

const _: () = assert!(core::mem::size_of::<(String, u64)>() == 32);

/// The absolute path of a directory, from [`Vfs::resolve_path`]'s two halves.
///
/// The form `created_dirs` is keyed on, which is [`Vfs::resolve_absolute`]'s.
/// One construction shared by the writer of that set and both its readers, so
/// they cannot drift into disagreeing about what a directory is called.
fn directory(mount: &str, subdir: &str) -> String {
    if subdir.is_empty() {
        format!("/{mount}")
    } else {
        format!("/{mount}/{subdir}")
    }
}

fn normalize(path: &str) -> String {
    // `parts` is the allocation MAX_PATH is derived against — see its comment.
    // Callers guarantee `path` is at most `MAX_PATH + 1 + MAX_USER_STR` bytes.
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => { parts.pop(); }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        String::from("/")
    } else {
        format!("/{}", parts.join("/"))
    }
}

impl Vfs {
    fn new() -> Self {
        Self {
            root: None,
            mounts: HashMap::new(),
            created_dirs: hashbrown::HashSet::new(),
        }
    }

    pub fn set_root(&mut self, fs: Box<dyn FileSystem>) {
        self.root = Some(fs);
    }

    pub fn root_mut(&mut self) -> &mut dyn FileSystem {
        self.root.as_deref_mut().expect("no root filesystem")
    }

    pub fn mount(&mut self, name: &str, fs: Box<dyn FileSystem>, access: UserAccess) {
        self.mounts.insert(String::from(name), Mount { fs, access });
    }

    /// May a syscall acting for userland change what is at `path`?
    ///
    /// The kernel's own answer is yes everywhere — it does not ask. This is
    /// the syscall layer's question, and it is asked by every syscall that can
    /// create, truncate, rename, delete or link, plus `open` for write.
    ///
    /// `/boot` is what it exists for. It is the volume firmware and the
    /// bootloader read the machine out of, and it had no permission model of
    /// any kind: `fs::write("/boot/toyos/kernel.elf", "TEETH")` from an
    /// ordinary process truncated the kernel image to five bytes, which is a
    /// machine that does not boot again. That is not a filesystem bug to fix
    /// in FAT32 — it is a mount that userland was never meant to be able to
    /// change.
    ///
    /// The root mount answers yes here and refuses in the adapter instead:
    /// the initrd is `ReadOnlyBcacheFsAdapter`, which has no write path to
    /// gate.
    pub fn user_may_modify(&self, path: &str) -> bool {
        let (mount, _) = self.resolve_path("/", path);
        self.mounts.get(&mount).map_or(true, |m| m.access == UserAccess::ReadWrite)
    }

    fn resolve_fs(&mut self, mount: &str, file: &str) -> Option<(&mut dyn FileSystem, String)> {
        if let Some(mount) = self.mounts.get_mut(mount) {
            return Some((mount.fs.as_mut(), String::from(file)));
        }
        if let Some(root) = self.root.as_deref_mut() {
            let root_path = if file.is_empty() {
                String::from(mount)
            } else {
                format!("{}/{}", mount, file)
            };
            return Some((root, root_path));
        }
        None
    }

    pub fn resolve_absolute(&self, cwd: &str, path: &str) -> String {
        if path.starts_with('/') {
            normalize(path)
        } else if cwd == "/" {
            normalize(&format!("/{}", path))
        } else {
            normalize(&format!("{}/{}", cwd, path))
        }
    }

    pub fn resolve_path(&self, cwd: &str, arg: &str) -> (String, String) {
        let full = if arg.starts_with('/') {
            normalize(arg)
        } else if cwd == "/" {
            normalize(&format!("/{}", arg))
        } else {
            normalize(&format!("{}/{}", cwd, arg))
        };

        if full == "/" {
            return (String::new(), String::new());
        }

        let without_leading = &full[1..];
        if let Some(pos) = without_leading.find('/') {
            let mount = &without_leading[..pos];
            let file = &without_leading[pos + 1..];
            (String::from(mount), String::from(file))
        } else {
            (String::from(without_leading), String::new())
        }
    }

    pub fn cd(&mut self, cwd: &str, target: &str) -> Result<String, SyscallError> {
        let (mount, subdir) = self.resolve_path(cwd, target);

        if mount.is_empty() {
            return Ok(String::from("/"));
        }

        let abs = directory(&mount, &subdir);

        // The one place a process's `cwd` is grown: `sys_chdir` stores whatever
        // this returns. Refused rather than truncated — a shortened path names a
        // *different* directory, and every later `resolve_absolute` against it
        // would silently resolve to the wrong file. Checked before the three
        // `Ok(abs)` returns below so none of them can hand back an over-long
        // path, and after `mount.is_empty()`, whose "/" is a byte long.
        if abs.len() > MAX_PATH {
            return Err(SyscallError::InvalidArgument);
        }

        if self.created_dirs.contains(&abs) {
            return Ok(abs);
        }

        let is_named = self.mounts.contains_key(&mount);
        if subdir.is_empty() && is_named {
            return Ok(abs);
        }

        let (fs, fs_path) = self.resolve_fs(&mount, &subdir).ok_or(SyscallError::NotFound)?;
        let prefix = format!("{}/", fs_path);
        // A mount too large to list is not a mount you can `cd` into either —
        // the answer would need the same allocation — and a mount that would
        // not answer is not one you can be told is absent.
        let names = fs.list(MAX_LIST_ENTRIES)?;
        if names.iter().any(|(name, _)| name.starts_with(&prefix) || *name == fs_path) {
            return Ok(abs);
        }

        Err(SyscallError::NotFound)
    }

    /// Every entry of one directory.
    ///
    /// Refuses above [`MAX_LIST_ENTRIES`] rather than truncating: a listing
    /// short of the truth is worse than no listing, because a caller
    /// enumerating a directory to delete it, or to check a name is absent,
    /// gets a confident wrong answer. The refusal reaches userland as
    /// `ResourceExhausted`.
    pub fn list(&mut self, cwd: &str, path: &str) -> Result<Vec<(String, u64)>, SyscallError> {
        let (mount, subdir) = if path.is_empty() {
            self.resolve_path(cwd, "")
        } else {
            self.resolve_path(cwd, path)
        };

        if mount.is_empty() {
            let mut result = Vec::new();
            let mut seen_dirs = hashbrown::HashSet::new();

            for name in self.mounts.keys() {
                let dir_name = format!("{}/", name);
                if seen_dirs.insert(dir_name.clone()) {
                    result.push((dir_name, 0));
                }
            }

            if let Some(root) = self.root.as_deref_mut() {
                for (name, _size) in root.list(MAX_LIST_ENTRIES)? {
                    if let Some(slash_pos) = name.find('/') {
                        let dir_name = format!("{}/", &name[..slash_pos]);
                        if seen_dirs.insert(dir_name.clone()) {
                            result.push((dir_name, 0));
                        }
                    }
                }
            }

            return Ok(result);
        }

        let (fs, fs_path) = self.resolve_fs(&mount, &subdir)
            .ok_or(SyscallError::NotFound)?;
        let all_files = fs.list(MAX_LIST_ENTRIES)?;

        let prefix = if fs_path.is_empty() {
            String::new()
        } else {
            format!("{}/", fs_path)
        };

        fn under_prefix<'a>(name: &'a str, prefix: &str) -> Option<&'a str> {
            if prefix.is_empty() { Some(name) } else { name.strip_prefix(prefix) }
        }

        // Counted first, then reserved exactly — the `elf.rs` shape. Growth by
        // doubling asks for the capacity it is moving *to*, so a `Vec` that
        // ends up holding N entries transiently requests up to `2N`, and it
        // was that overshoot rather than the final size that crossed the heap
        // ceiling. Dedup only removes entries, so this is an upper bound.
        let matching = all_files.iter().filter(|(n, _)| under_prefix(n, &prefix).is_some()).count();
        let mut result = Vec::with_capacity(matching);
        let mut seen_dirs = hashbrown::HashSet::new();

        for (name, size) in &all_files {
            let Some(rest) = under_prefix(name, &prefix) else { continue };

            if let Some(slash_pos) = rest.find('/') {
                let dir_name = format!("{}/", &rest[..slash_pos]);
                if seen_dirs.insert(dir_name.clone()) {
                    result.push((dir_name, 0));
                }
            } else {
                result.push((String::from(rest), *size));
            }
        }

        // An empty directory and a path no directory could be are different
        // answers, and this is the only place that can tell them apart. A
        // directory here exists for one of three reasons — it is a mount, some
        // file lives under it, or something called `mkdir` — and only the first
        // two are visible in `result`. Without the third, `mkdir("/tmp/d")`
        // followed by `read_dir("/tmp/d")` was `NotFound`, and `is_dir` read the
        // same refusal for an empty `d` as for a `d` that was never there: which
        // is how `cp x d/` came to write a *file* named `d`.
        if result.is_empty() && !prefix.is_empty() && !self.created_dirs.contains(&directory(&mount, &subdir)) {
            return Err(SyscallError::NotFound);
        }
        Ok(result)
    }

    /// Open a file for fd-based I/O.
    ///
    /// The backing the filesystem hands back is registered with the file
    /// cache rather than returned: it belongs to the file, not to the fd that
    /// happened to open it, and eviction is only sound for pages the cache
    /// itself knows how to fetch again.
    pub fn open_file(&mut self, path: &str) -> Result<FileId, SyscallError> {
        let (file_id, backing) = self.open_file_depth(path, 0)?;
        if let Some(backing) = backing {
            crate::file_cache::set_backing(file_id, backing);
        }
        Ok(file_id)
    }

    fn open_file_depth(&mut self, path: &str, depth: u32) -> Result<(FileId, Option<alloc::sync::Arc<dyn crate::file_backing::FileBacking>>), SyscallError> {
        if depth > 10 { return Err(SyscallError::InvalidArgument); }
        let (mount, file) = self.resolve_path("/", path);
        if mount.is_empty() { return Err(SyscallError::NotFound); }
        let is_named = self.mounts.contains_key(&mount);
        let (fs, fs_path) = self.resolve_fs(&mount, &file).ok_or(SyscallError::NotFound)?;
        if fs_path.is_empty() { return Err(SyscallError::NotFound); }
        if let Some(target) = fs.read_link(&fs_path)? {
            let resolved = if is_named {
                format!("/{}/{}", mount, target)
            } else {
                format!("/{}", target)
            };
            return self.open_file_depth(&resolved, depth + 1);
        }
        fs.open_file(&fs_path)
    }

    /// Create a new empty file. Returns FileId.
    pub fn create_file(&mut self, path: &str, mtime: u64) -> Result<FileId, SyscallError> {
        let (mount, file) = self.resolve_path("/", path);
        if mount.is_empty() { return Err(SyscallError::InvalidArgument); }
        let (fs, fs_path) = self.resolve_fs(&mount, &file).ok_or(SyscallError::NotFound)?;
        if fs_path.is_empty() { return Err(SyscallError::InvalidArgument); }
        fs.create(&fs_path, mtime)
    }

    /// Flush dirty pages for a file, then update metadata.
    ///
    /// No early return on an empty dirty set. A `ftruncate` changes the file's
    /// size without dirtying a page, so returning here left the new size in the
    /// file cache and never told the filesystem — correct until the last fd
    /// closed and the cached size went with it. Callers reach this only when
    /// the fd is marked modified, so there is always something to record.
    pub fn flush_file(&mut self, path: &str, file_id: FileId, mtime: u64) -> Result<(), SyscallError> {
        let dirty = crate::file_cache::clone_dirty(file_id);

        let (mount, file) = self.resolve_path("/", path);
        if mount.is_empty() { return Err(SyscallError::InvalidArgument); }
        let (fs, fs_path) = self.resolve_fs(&mount, &file).ok_or(SyscallError::NotFound)?;
        if fs_path.is_empty() { return Err(SyscallError::InvalidArgument); }

        // On the heap and not the stack. `log_file` reaches this from the idle
        // loop, whose per-CPU stack is 16 KiB of ordinary heap with no guard
        // page — so a 4 KiB frame there is a quarter of the stack and an
        // overflow corrupts whatever the allocator put underneath it, silently.
        // Measured at that call site: 11,505 bytes of the 16,384 in use at the
        // block layer, with the USB command path still below. `Vec` rather than
        // `Box::new([0u8; 4096])`, because the latter is only elided from the
        // stack if the optimiser feels like it.
        let mut heap = alloc::vec![0u8; 4096].into_boxed_slice();
        let buf: &mut [u8; 4096] = (&mut heap[..]).try_into().expect("4096 bytes");
        for &page_idx in &dirty {
            // A truncate on another CPU can take a page out of the cache
            // between `clone_dirty` and here. Writing whatever the buffer
            // happens to hold would put bytes in the file no writer produced.
            if crate::file_cache::copy_page_out(file_id, page_idx, buf) {
                fs.write_page(file_id, page_idx, buf)?;
            }
        }
        crate::file_cache::clear_dirty(file_id, &dirty);

        let size = crate::file_cache::size(file_id);
        fs.update_metadata(file_id, size, mtime)?;

        // A file created in this boot had no blocks to point a backing at,
        // so its pages were unevictable up to here. They are on disk now.
        //
        // A refusal here is not the flush's: the bytes reached the device two
        // statements ago. It costs this file its evictability for the rest of
        // the boot, which is a cost the caller has nothing to do about.
        if !crate::file_cache::has_backing(file_id) {
            match fs.open_backing(&fs_path) {
                Ok(backing) => crate::file_cache::set_backing(file_id, backing),
                Err(e) => log!("vfs: {path} was flushed but has no backing to evict through: {e}"),
            }
        }
        Ok(())
    }

    /// Close a file (release filesystem state when last ref drops).
    pub fn close_file(&mut self, path: &str, file_id: FileId) {
        let (mount, file) = self.resolve_path("/", path);
        if mount.is_empty() { return; }
        if let Some((fs, _fs_path)) = self.resolve_fs(&mount, &file) {
            fs.close_file(file_id);
        }
    }

    /// Delete a file. Handles file cache mark_deleted for the FileId.
    pub fn delete_file(&mut self, path: &str) -> Result<(), SyscallError> {
        let (mount, file) = self.resolve_path("/", path);
        if mount.is_empty() { return Err(SyscallError::InvalidArgument); }
        let (fs, fs_path) = self.resolve_fs(&mount, &file).ok_or(SyscallError::NotFound)?;
        if fs_path.is_empty() { return Err(SyscallError::InvalidArgument); }
        fs.delete(&fs_path)
    }

    pub fn file_mtime(&mut self, path: &str) -> Result<u64, SyscallError> {
        self.file_mtime_depth(path, 0)
    }

    fn file_mtime_depth(&mut self, path: &str, depth: u32) -> Result<u64, SyscallError> {
        if depth > 10 { return Err(SyscallError::InvalidArgument); }
        let (mount, file) = self.resolve_path("/", path);
        if mount.is_empty() { return Err(SyscallError::NotFound); }
        let is_named = self.mounts.contains_key(&mount);
        let (fs, fs_path) = self.resolve_fs(&mount, &file).ok_or(SyscallError::NotFound)?;
        if fs_path.is_empty() { return Err(SyscallError::NotFound); }
        if let Some(target) = fs.read_link(&fs_path)? {
            let resolved = if is_named {
                format!("/{}/{}", mount, target)
            } else {
                format!("/{}", target)
            };
            return self.file_mtime_depth(&resolved, depth + 1);
        }
        fs.file_mtime(&fs_path)
    }

    pub fn rename(&mut self, old_path: &str, new_path: &str) -> Result<(), SyscallError> {
        let (old_mount, old_file) = self.resolve_path("/", old_path);
        let (new_mount, new_file) = self.resolve_path("/", new_path);
        if old_mount.is_empty() || new_mount.is_empty() { return Err(SyscallError::InvalidArgument); }
        if old_mount != new_mount { return Err(SyscallError::NotSupported); }
        let is_named = self.mounts.contains_key(&old_mount);
        let (fs, old_fs_path) = self.resolve_fs(&old_mount, &old_file).ok_or(SyscallError::NotFound)?;
        let new_fs_path = if is_named {
            String::from(&new_file)
        } else if new_file.is_empty() {
            String::from(&new_mount)
        } else {
            format!("{}/{}", new_mount, new_file)
        };
        if old_fs_path.is_empty() || new_fs_path.is_empty() { return Err(SyscallError::InvalidArgument); }
        fs.rename(&old_fs_path, &new_fs_path)
    }

    /// Record a directory, or refuse a path no directory could have.
    ///
    /// `cd` bounds what it returns by `MAX_PATH`, so a longer path names a
    /// directory nothing could ever chdir into. Storing one would grow
    /// `created_dirs` for a name that is unreachable by construction, and would
    /// make `cd`'s `None` a lie — it would be reporting "no such directory" for
    /// something this function had just accepted.
    ///
    /// The `Result` is the point as much as the bound is: `sys_mkdir` used to
    /// discard this outcome and report success unconditionally, so a bound
    /// added here without changing the return would have been a *silent*
    /// failure — the caller told nothing, the directory simply absent.
    pub fn create_dir(&mut self, path: &str) -> Result<(), SyscallError> {
        if path.len() > MAX_PATH {
            return Err(SyscallError::InvalidArgument);
        }
        self.created_dirs.insert(String::from(path));
        Ok(())
    }

    pub fn remove_dir(&mut self, path: &str) {
        self.created_dirs.remove(path);
        let prefix = format!("{}/", path);
        self.created_dirs.retain(|d| !d.starts_with(&prefix));
    }

    pub fn create_symlink(&mut self, path: &str, target: &str) -> Result<(), SyscallError> {
        let (mount, file) = self.resolve_path("/", path);
        if mount.is_empty() {
            return Err(SyscallError::InvalidArgument);
        }
        let (fs, fs_path) = self.resolve_fs(&mount, &file).ok_or(SyscallError::NotFound)?;
        if fs_path.is_empty() { return Err(SyscallError::InvalidArgument); }
        fs.create_symlink(&fs_path, target)
    }

    pub fn read_link(&mut self, path: &str) -> Result<Option<String>, SyscallError> {
        let (mount, file) = self.resolve_path("/", path);
        if mount.is_empty() {
            return Ok(None);
        }
        let Some((fs, fs_path)) = self.resolve_fs(&mount, &file) else { return Ok(None) };
        if fs_path.is_empty() { return Ok(None); }
        fs.read_link(&fs_path)
    }

    pub fn delete(&mut self, path: &str) -> Result<(), SyscallError> {
        self.delete_file(path)
    }

    /// Make one mount's writes durable.
    ///
    /// [`Vfs::sync_all`] is the wrong tool for a caller that knows which
    /// filesystem it wrote to: on a machine with a `/home` on NVMe it is a
    /// btree write-back and a device flush for a byte that went to the boot
    /// stick.
    pub fn sync_mount(&mut self, name: &str) -> Result<(), SyscallError> {
        self.mounts.get_mut(name).ok_or(SyscallError::NotFound)?.fs.sync()
    }

    /// Every mount, on the way down. Failures are logged here and not returned:
    /// the caller is `SYS_SHUTDOWN`, which has nowhere to put a `Result` and
    /// nothing left to try, and one mount refusing must not stop the rest from
    /// being written out.
    pub fn sync_all(&mut self) {
        if let Some(root) = &mut self.root {
            if let Err(e) = root.sync() {
                log!("vfs: the root filesystem would not sync: {e}");
            }
        }
        for (name, mount) in self.mounts.iter_mut() {
            if let Err(e) = mount.fs.sync() {
                log!("vfs: /{name} would not sync: {e}");
            }
        }
    }

    /// Open a file backing for demand-paged ELF loading.
    /// This is separate from fd-based I/O and doesn't use the file cache.
    pub fn open_backing(&mut self, path: &str) -> Result<alloc::sync::Arc<dyn crate::file_backing::FileBacking>, SyscallError> {
        self.open_backing_depth(path, 0)
    }

    fn open_backing_depth(&mut self, path: &str, depth: u32) -> Result<alloc::sync::Arc<dyn crate::file_backing::FileBacking>, SyscallError> {
        if depth > 10 { return Err(SyscallError::InvalidArgument); }
        let (mount, file) = self.resolve_path("/", path);
        if mount.is_empty() { return Err(SyscallError::NotFound); }
        let is_named = self.mounts.contains_key(&mount);
        let (fs, fs_path) = self.resolve_fs(&mount, &file).ok_or(SyscallError::NotFound)?;
        if fs_path.is_empty() { return Err(SyscallError::NotFound); }
        if let Some(target) = fs.read_link(&fs_path)? {
            let resolved = if is_named {
                format!("/{}/{}", mount, target)
            } else {
                format!("/{}", target)
            };
            return self.open_backing_depth(&resolved, depth + 1);
        }
        fs.open_backing(&fs_path)
    }
}
