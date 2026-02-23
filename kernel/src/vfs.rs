use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::sync::SyncCell;

static VFS: SyncCell<Option<Vfs>> = SyncCell::new(None);

/// Store the VFS in a global static (takes ownership).
pub fn set_global(vfs: Vfs) {
    *VFS.get_mut() = Some(vfs);
}

/// Get a mutable reference to the global VFS.
pub fn global() -> &'static mut Vfs {
    VFS.get_mut().as_mut().expect("VFS not initialized")
}

/// Trait abstracting filesystem operations so the VFS can hold
/// heterogeneous mount points (initrd on SliceDisk, nvme on NvmeDisk).
pub trait FileSystem {
    fn list(&mut self) -> Vec<(String, u64)>;
    fn read_file(&mut self, name: &str) -> Option<Vec<u8>>;
    fn create(&mut self, name: &str, data: &[u8]) -> bool;
    fn delete(&mut self, name: &str) -> bool;
}

impl<D: tyfs::Disk> FileSystem for tyfs::SimpleFs<D> {
    fn list(&mut self) -> Vec<(String, u64)> {
        tyfs::SimpleFs::list(self)
    }
    fn read_file(&mut self, name: &str) -> Option<Vec<u8>> {
        tyfs::SimpleFs::read_file(self, name)
    }
    fn create(&mut self, name: &str, data: &[u8]) -> bool {
        tyfs::SimpleFs::create(self, name, data)
    }
    fn delete(&mut self, name: &str) -> bool {
        tyfs::SimpleFs::delete(self, name)
    }
}

struct Mount {
    name: String,
    fs: Box<dyn FileSystem>,
}

/// Virtual filesystem that dispatches to named mount points.
/// Subdirectories are virtual — TYFS stores flat filenames with `/` separators.
pub struct Vfs {
    mounts: Vec<Mount>,
}

/// Normalize a path by resolving `.` and `..` components.
/// Always returns an absolute path starting with `/`.
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
    pub fn new() -> Self {
        Self {
            mounts: Vec::new(),
        }
    }

    pub fn mount(&mut self, name: &str, fs: Box<dyn FileSystem>) {
        self.mounts.push(Mount {
            name: String::from(name),
            fs,
        });
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

        if self.find_mount(&mount).is_none() {
            return None;
        }

        if subdir.is_empty() {
            return Some(format!("/{}", mount));
        }

        // Check if any files exist with this subdirectory prefix
        let prefix = format!("{}/", subdir);
        if let Some(m) = self.find_mount_mut(&mount) {
            if m.fs.list().iter().any(|(name, _)| name.starts_with(&prefix)) {
                return Some(format!("/{}/{}", mount, subdir));
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
            let names: Vec<(String, u64)> = self
                .mounts
                .iter()
                .map(|m| (format!("{}/", m.name), 0))
                .collect();
            return Ok(names);
        }

        let m = self.mounts.iter_mut().find(|m| m.name == mount)
            .ok_or("no such directory")?;
        let all_files = m.fs.list();

        let prefix = if subdir.is_empty() {
            String::new()
        } else {
            format!("{}/", subdir)
        };

        let mut result = Vec::new();
        let mut seen_dirs: Vec<String> = Vec::new();

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
                if !seen_dirs.contains(&dir_name) {
                    seen_dirs.push(dir_name.clone());
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

    pub fn read_file(&mut self, path: &str) -> Option<Vec<u8>> {
        // read_file always takes absolute paths
        let (mount, file) = self.resolve_path("/", path);
        if file.is_empty() {
            return None;
        }
        let m = self.find_mount_mut(&mount)?;
        m.fs.read_file(&file)
    }

    pub fn write_file(&mut self, path: &str, data: &[u8]) -> bool {
        let (mount, file) = self.resolve_path("/", path);
        if file.is_empty() {
            return false;
        }
        if let Some(m) = self.find_mount_mut(&mount) {
            m.fs.delete(&file);
            m.fs.create(&file, data)
        } else {
            false
        }
    }

    pub fn delete(&mut self, path: &str) -> bool {
        let (mount, file) = self.resolve_path("/", path);
        if file.is_empty() {
            return false;
        }
        if let Some(m) = self.find_mount_mut(&mount) {
            m.fs.delete(&file)
        } else {
            false
        }
    }

    pub fn mount_exists(&self, name: &str) -> bool {
        self.find_mount(name).is_some()
    }

    fn find_mount(&self, name: &str) -> Option<&Mount> {
        self.mounts.iter().find(|m| m.name == name)
    }

    fn find_mount_mut(&mut self, name: &str) -> Option<&mut Mount> {
        self.mounts.iter_mut().find(|m| m.name == name)
    }
}
