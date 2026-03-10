use alloc::borrow::Cow;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use hashbrown::{HashMap, HashSet};

use core::ops::{Deref, DerefMut};
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
    fn read_file(&mut self, name: &str) -> Result<Cow<'static, [u8]>, &'static str>;
    fn read_link(&mut self, name: &str) -> Option<String>;
    fn file_mtime(&mut self, name: &str) -> u64;
    fn create(&mut self, name: &str, data: &[u8], mtime: u64) -> Result<(), &'static str>;
    fn delete(&mut self, name: &str) -> bool;
    fn delete_prefix(&mut self, prefix: &str);
    fn create_symlink(&mut self, name: &str, target: &str) -> Result<(), &'static str>;
    fn sync(&mut self);
    /// Return disk block numbers for each 4KB block of a file.
    /// Only supported by block-device-backed filesystems (ToyFS).
    fn file_block_map(&mut self, _name: &str) -> Option<Vec<u64>> { None }
}


/// Virtual filesystem that dispatches to named mount points.
/// Subdirectories are virtual — TYFS stores flat filenames with `/` separators.
pub struct Vfs {
    root: Option<Box<dyn FileSystem>>,
    mounts: HashMap<String, Box<dyn FileSystem>>,
    created_dirs: HashSet<String>,
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
            created_dirs: HashSet::new(),
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

    /// Get the filesystem for a mount name: named mounts take priority, then root.
    /// Returns the filesystem and the path to use within it.
    fn resolve_fs(&mut self, mount: &str, file: &str) -> Option<(&mut dyn FileSystem, String)> {
        if let Some(fs) = self.mounts.get_mut(mount) {
            return Some((fs.as_mut(), String::from(file)));
        }
        if let Some(root) = self.root.as_deref_mut() {
            let root_path = if file.is_empty() {
                String::from(mount)
            } else {
                alloc::format!("{}/{}", mount, file)
            };
            return Some((root, root_path));
        }
        None
    }

    /// Resolve a (possibly relative) path against the given cwd.
    /// Returns the absolute normalized path.
    pub fn resolve_absolute(&self, cwd: &str, path: &str) -> String {
        if path.starts_with('/') {
            normalize(path)
        } else if cwd == "/" {
            normalize(&format!("/{}", path))
        } else {
            normalize(&format!("{}/{}", cwd, path))
        }
    }

    /// Resolve a (possibly relative) path against the given cwd.
    /// Returns `(mount_name, filename)`. An empty mount means root.
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

    /// Check if a directory target exists. Returns the new absolute cwd, or None.
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

        // Check explicitly created directories
        if self.created_dirs.contains(&abs) {
            return Some(abs);
        }

        // Named mount with no subdir always exists
        let is_named = self.mounts.contains_key(&mount);
        if subdir.is_empty() && is_named {
            return Some(abs);
        }

        if let Some((fs, fs_path)) = self.resolve_fs(&mount, &subdir) {
            // Check if any files exist with this subdirectory prefix
            let prefix = format!("{}/", fs_path);
            if fs.list().iter().any(|(name, _)| name.starts_with(&prefix) || *name == fs_path) {
                return Some(abs);
            }
        }

        None
    }

    /// List entries at a path. Returns files and virtual subdirectories.
    pub fn list(&mut self, cwd: &str, path: &str) -> Result<Vec<(String, u64)>, &'static str> {
        let (mount, subdir) = if path.is_empty() {
            self.resolve_path(cwd, "")
        } else {
            self.resolve_path(cwd, path)
        };

        if mount.is_empty() {
            // Root listing: top-level dirs from root fs + named mount names
            let mut result = Vec::new();
            let mut seen_dirs = HashSet::new();

            // Named mounts
            for name in self.mounts.keys() {
                let dir_name = format!("{}/", name);
                if seen_dirs.insert(dir_name.clone()) {
                    result.push((dir_name, 0));
                }
            }

            // Top-level entries from root filesystem
            if let Some(root) = self.root.as_deref_mut() {
                for (name, _size) in root.list() {
                    if let Some(slash_pos) = name.find('/') {
                        let dir_name = format!("{}/", &name[..slash_pos]);
                        if seen_dirs.insert(dir_name.clone()) {
                            result.push((dir_name, 0));
                        }
                    }
                    // Don't show root-level files (there shouldn't be any)
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
        let mut seen_dirs = HashSet::new();

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

    pub fn read_file(&mut self, path: &str) -> Result<Cow<'static, [u8]>, &'static str> {
        self.read_file_depth(path, 0)
    }

    fn read_file_depth(&mut self, path: &str, depth: u32) -> Result<Cow<'static, [u8]>, &'static str> {
        if depth > 10 { return Err("too many symlinks"); }
        let (mount, file) = self.resolve_path("/", path);
        if mount.is_empty() {
            return Err("not found");
        }
        let is_named = self.mounts.contains_key(&mount);
        let (fs, fs_path) = self.resolve_fs(&mount, &file).ok_or("not found")?;
        if fs_path.is_empty() {
            return Err("not found");
        }
        if let Some(target) = fs.read_link(&fs_path) {
            // For named mounts, resolve relative to the mount point.
            // For root fs, the target is already a root-relative path.
            let resolved = if is_named {
                format!("/{}/{}", mount, target)
            } else {
                format!("/{}", target)
            };
            return self.read_file_depth(&resolved, depth + 1);
        }
        fs.read_file(&fs_path)
    }

    pub fn write_file(&mut self, path: &str, data: &[u8], mtime: u64) -> Result<(), &'static str> {
        let (mount, file) = self.resolve_path("/", path);
        if mount.is_empty() {
            return Err("cannot write to root");
        }
        let (fs, fs_path) = self.resolve_fs(&mount, &file).ok_or("no filesystem")?;
        if fs_path.is_empty() { return Err("invalid path"); }
        fs.delete(&fs_path);
        fs.create(&fs_path, data, mtime)
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
            alloc::format!("{}/{}", new_mount, new_file)
        };
        if old_fs_path.is_empty() || new_fs_path.is_empty() { return Err("invalid path"); }
        let data = fs.read_file(&old_fs_path)?;
        let mtime = fs.file_mtime(&old_fs_path);
        fs.delete(&new_fs_path);
        fs.create(&new_fs_path, &data, mtime)?;
        fs.delete(&old_fs_path);
        Ok(())
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
        let (mount, file) = self.resolve_path("/", path);
        if mount.is_empty() {
            return false;
        }
        if let Some((fs, fs_path)) = self.resolve_fs(&mount, &file) {
            if fs_path.is_empty() { return false; }
            fs.delete(&fs_path)
        } else {
            false
        }
    }

    /// Return disk block numbers for a file. Only works for block-device-backed files.
    /// Follows symlinks (up to 10 levels).
    pub fn file_block_map(&mut self, path: &str) -> Option<Vec<u64>> {
        self.file_block_map_depth(path, 0)
    }

    fn file_block_map_depth(&mut self, path: &str, depth: u32) -> Option<Vec<u64>> {
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
            return self.file_block_map_depth(&resolved, depth + 1);
        }
        fs.file_block_map(&fs_path)
    }

}
