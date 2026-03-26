use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use hashbrown::HashMap;

use core::ops::{Deref, DerefMut};
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

/// Trait abstracting filesystem operations so the VFS can hold
/// heterogeneous mount points (initrd on SliceDisk, nvme on NvmeDisk).
pub trait FileSystem: Send {
    fn list(&mut self) -> Vec<(String, u64)>;
    fn file_size(&mut self, name: &str) -> Option<u64>;
    fn file_mtime(&mut self, name: &str) -> u64;
    fn read_link(&mut self, name: &str) -> Option<String>;

    /// Open a file. Returns (FileId, optional backing for cache misses).
    /// Must return the SAME FileId for the same file across multiple opens.
    fn open_file(&mut self, name: &str) -> Option<(FileId, Option<alloc::sync::Arc<dyn crate::file_backing::FileBacking>>)>;
    /// Create an empty file. Returns FileId. Registers in name→FileId map.
    fn create(&mut self, name: &str, mtime: u64) -> Result<FileId, &'static str>;
    /// Release filesystem-side state for a FileId (called when ref_count reaches 0).
    fn close_file(&mut self, file_id: FileId);

    fn delete(&mut self, name: &str) -> bool;
    fn delete_prefix(&mut self, prefix: &str);
    fn rename(&mut self, old: &str, new: &str) -> Result<(), &'static str>;

    /// Write a single dirty page to persistent storage. The filesystem resolves
    /// page_idx to a disk block (allocating if needed).
    fn write_page(&mut self, file_id: FileId, page_idx: u32, data: &[u8; 4096]) -> Result<(), &'static str>;
    /// Update file metadata (size, mtime) after flushing dirty pages.
    fn update_metadata(&mut self, file_id: FileId, size: u64, mtime: u64) -> Result<(), &'static str>;

    fn create_symlink(&mut self, name: &str, target: &str) -> Result<(), &'static str>;
    fn sync(&mut self);

    /// Open a file backing for demand-paged ELF loading (separate from fd I/O).
    fn open_backing(&mut self, _name: &str) -> Option<alloc::sync::Arc<dyn crate::file_backing::FileBacking>> { None }
}


/// Virtual filesystem that dispatches to named mount points.
pub struct Vfs {
    root: Option<Box<dyn FileSystem>>,
    mounts: HashMap<String, Box<dyn FileSystem>>,
    created_dirs: hashbrown::HashSet<String>,
}

fn normalize(path: &str) -> String {
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

    pub fn mount(&mut self, name: &str, fs: Box<dyn FileSystem>) {
        self.mounts.insert(String::from(name), fs);
    }

    fn resolve_fs(&mut self, mount: &str, file: &str) -> Option<(&mut dyn FileSystem, String)> {
        if let Some(fs) = self.mounts.get_mut(mount) {
            return Some((fs.as_mut(), String::from(file)));
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

    pub fn cd(&mut self, cwd: &str, target: &str) -> Option<String> {
        let (mount, subdir) = self.resolve_path(cwd, target);

        if mount.is_empty() {
            return Some(String::from("/"));
        }

        let abs = if subdir.is_empty() {
            format!("/{}", mount)
        } else {
            format!("/{}/{}", mount, subdir)
        };

        if self.created_dirs.contains(&abs) {
            return Some(abs);
        }

        let is_named = self.mounts.contains_key(&mount);
        if subdir.is_empty() && is_named {
            return Some(abs);
        }

        if let Some((fs, fs_path)) = self.resolve_fs(&mount, &subdir) {
            let prefix = format!("{}/", fs_path);
            if fs.list().iter().any(|(name, _)| name.starts_with(&prefix) || *name == fs_path) {
                return Some(abs);
            }
        }

        None
    }

    pub fn list(&mut self, cwd: &str, path: &str) -> Result<Vec<(String, u64)>, &'static str> {
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
                for (name, _size) in root.list() {
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
            .ok_or("no such directory")?;
        let all_files = fs.list();

        let prefix = if fs_path.is_empty() {
            String::new()
        } else {
            format!("{}/", fs_path)
        };

        let mut result = Vec::new();
        let mut seen_dirs = hashbrown::HashSet::new();

        for (name, size) in &all_files {
            let rest = if prefix.is_empty() {
                name.as_str()
            } else if let Some(r) = name.strip_prefix(prefix.as_str()) {
                r
            } else {
                continue;
            };

            if let Some(slash_pos) = rest.find('/') {
                let dir_name = format!("{}/", &rest[..slash_pos]);
                if seen_dirs.insert(dir_name.clone()) {
                    result.push((dir_name, 0));
                }
            } else {
                result.push((String::from(rest), *size));
            }
        }

        if !prefix.is_empty() && result.is_empty() {
            Err("no such directory")
        } else {
            Ok(result)
        }
    }

    /// Open a file for fd-based I/O. Returns (FileId, optional backing).
    pub fn open_file(&mut self, path: &str) -> Option<(FileId, Option<alloc::sync::Arc<dyn crate::file_backing::FileBacking>>)> {
        self.open_file_depth(path, 0)
    }

    fn open_file_depth(&mut self, path: &str, depth: u32) -> Option<(FileId, Option<alloc::sync::Arc<dyn crate::file_backing::FileBacking>>)> {
        if depth > 10 { return None; }
        let (mount, file) = self.resolve_path("/", path);
        if mount.is_empty() { return None; }
        let is_named = self.mounts.contains_key(&mount);
        let (fs, fs_path) = self.resolve_fs(&mount, &file)?;
        if fs_path.is_empty() { return None; }
        if let Some(target) = fs.read_link(&fs_path) {
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
    pub fn create_file(&mut self, path: &str, mtime: u64) -> Result<FileId, &'static str> {
        let (mount, file) = self.resolve_path("/", path);
        if mount.is_empty() { return Err("cannot create at root"); }
        let (fs, fs_path) = self.resolve_fs(&mount, &file).ok_or("no filesystem")?;
        if fs_path.is_empty() { return Err("invalid path"); }
        fs.create(&fs_path, mtime)
    }

    /// Flush dirty pages for a file, then update metadata.
    pub fn flush_file(&mut self, path: &str, file_id: FileId, mtime: u64) -> Result<(), &'static str> {
        let dirty = crate::file_cache::clone_dirty(file_id);
        if dirty.is_empty() {
            return Ok(());
        }

        let (mount, file) = self.resolve_path("/", path);
        if mount.is_empty() { return Err("invalid path"); }
        let (fs, fs_path) = self.resolve_fs(&mount, &file).ok_or("no filesystem")?;
        if fs_path.is_empty() { return Err("invalid path"); }

        let mut buf = [0u8; 4096];
        for &page_idx in &dirty {
            crate::file_cache::copy_page_out(file_id, page_idx, &mut buf);
            fs.write_page(file_id, page_idx, &buf)?;
        }
        crate::file_cache::clear_dirty(file_id);

        let size = crate::file_cache::size(file_id);
        fs.update_metadata(file_id, size, mtime)
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
    pub fn delete_file(&mut self, path: &str) -> bool {
        let (mount, file) = self.resolve_path("/", path);
        if mount.is_empty() { return false; }
        if let Some((fs, fs_path)) = self.resolve_fs(&mount, &file) {
            if fs_path.is_empty() { return false; }
            fs.delete(&fs_path)
        } else {
            false
        }
    }

    pub fn file_mtime(&mut self, path: &str) -> u64 {
        self.file_mtime_depth(path, 0)
    }

    fn file_mtime_depth(&mut self, path: &str, depth: u32) -> u64 {
        if depth > 10 { return 0; }
        let (mount, file) = self.resolve_path("/", path);
        if mount.is_empty() { return 0; }
        let is_named = self.mounts.contains_key(&mount);
        if let Some((fs, fs_path)) = self.resolve_fs(&mount, &file) {
            if fs_path.is_empty() { return 0; }
            if let Some(target) = fs.read_link(&fs_path) {
                let resolved = if is_named {
                    format!("/{}/{}", mount, target)
                } else {
                    format!("/{}", target)
                };
                return self.file_mtime_depth(&resolved, depth + 1);
            }
            fs.file_mtime(&fs_path)
        } else {
            0
        }
    }

    pub fn rename(&mut self, old_path: &str, new_path: &str) -> Result<(), &'static str> {
        let (old_mount, old_file) = self.resolve_path("/", old_path);
        let (new_mount, new_file) = self.resolve_path("/", new_path);
        if old_mount.is_empty() || new_mount.is_empty() { return Err("invalid path"); }
        if old_mount != new_mount { return Err("cross-mount rename"); }
        let is_named = self.mounts.contains_key(&old_mount);
        let Some((fs, old_fs_path)) = self.resolve_fs(&old_mount, &old_file) else { return Err("no filesystem") };
        let new_fs_path = if is_named {
            String::from(&new_file)
        } else if new_file.is_empty() {
            String::from(&new_mount)
        } else {
            format!("{}/{}", new_mount, new_file)
        };
        if old_fs_path.is_empty() || new_fs_path.is_empty() { return Err("invalid path"); }
        fs.rename(&old_fs_path, &new_fs_path)
    }

    pub fn create_dir(&mut self, path: &str) {
        self.created_dirs.insert(String::from(path));
    }

    pub fn remove_dir(&mut self, path: &str) {
        self.created_dirs.remove(path);
        let prefix = format!("{}/", path);
        self.created_dirs.retain(|d| !d.starts_with(&prefix));
    }

    pub fn create_symlink(&mut self, path: &str, target: &str) -> Result<(), &'static str> {
        let (mount, file) = self.resolve_path("/", path);
        if mount.is_empty() {
            return Err("cannot create symlink at root");
        }
        let (fs, fs_path) = self.resolve_fs(&mount, &file).ok_or("no filesystem")?;
        if fs_path.is_empty() { return Err("invalid path"); }
        fs.create_symlink(&fs_path, target)
    }

    pub fn read_link(&mut self, path: &str) -> Option<String> {
        let (mount, file) = self.resolve_path("/", path);
        if mount.is_empty() {
            return None;
        }
        let (fs, fs_path) = self.resolve_fs(&mount, &file)?;
        if fs_path.is_empty() { return None; }
        fs.read_link(&fs_path)
    }

    pub fn delete(&mut self, path: &str) -> bool {
        self.delete_file(path)
    }

    pub fn sync_all(&mut self) {
        if let Some(root) = &mut self.root {
            root.sync();
        }
        for fs in self.mounts.values_mut() {
            fs.sync();
        }
    }

    /// Open a file backing for demand-paged ELF loading.
    /// This is separate from fd-based I/O and doesn't use the file cache.
    pub fn open_backing(&mut self, path: &str) -> Option<alloc::sync::Arc<dyn crate::file_backing::FileBacking>> {
        self.open_backing_depth(path, 0)
    }

    fn open_backing_depth(&mut self, path: &str, depth: u32) -> Option<alloc::sync::Arc<dyn crate::file_backing::FileBacking>> {
        if depth > 10 { return None; }
        let (mount, file) = self.resolve_path("/", path);
        if mount.is_empty() { return None; }
        let is_named = self.mounts.contains_key(&mount);
        let (fs, fs_path) = self.resolve_fs(&mount, &file)?;
        if fs_path.is_empty() { return None; }
        if let Some(target) = fs.read_link(&fs_path) {
            let resolved = if is_named {
                format!("/{}/{}", mount, target)
            } else {
                format!("/{}", target)
            };
            return self.open_backing_depth(&resolved, depth + 1);
        }
        fs.open_backing(&fs_path)
    }

    /// Get file size. For open files, use file_cache::size() instead.
    pub fn file_size(&mut self, path: &str) -> Option<u64> {
        let (mount, file) = self.resolve_path("/", path);
        if mount.is_empty() { return None; }
        let (fs, fs_path) = self.resolve_fs(&mount, &file)?;
        if fs_path.is_empty() { return None; }
        fs.file_size(&fs_path)
    }
}
