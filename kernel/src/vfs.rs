use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

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
pub struct Vfs {
    mounts: Vec<Mount>,
    cwd: String,
}

impl Vfs {
    pub fn new() -> Self {
        Self {
            mounts: Vec::new(),
            cwd: String::from("/"),
        }
    }

    pub fn mount(&mut self, name: &str, fs: Box<dyn FileSystem>) {
        self.mounts.push(Mount {
            name: String::from(name),
            fs,
        });
    }

    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    /// Resolve a (possibly relative) path against the cwd.
    /// Returns `(mount_name, filename)`. An empty mount means root.
    pub fn resolve_path(&self, arg: &str) -> (String, String) {
        let full = if arg.starts_with('/') {
            String::from(arg)
        } else if self.cwd == "/" {
            format!("/{}", arg)
        } else {
            format!("{}/{}", self.cwd, arg)
        };

        let full = full.trim_end_matches('/');
        if full.is_empty() {
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

    /// Change directory. Returns false if the target doesn't exist.
    pub fn cd(&mut self, target: &str) -> bool {
        if target == "/" || target == ".." {
            self.cwd = String::from("/");
            return true;
        }
        let (mount, _) = self.resolve_path(target);
        if self.find_mount(&mount).is_some() {
            self.cwd = format!("/{}", mount);
            true
        } else {
            false
        }
    }

    /// List mount points (root) or files within a mount.
    pub fn list(&mut self, path: &str) -> Result<Vec<(String, u64)>, &'static str> {
        let (mount, _) = if path.is_empty() {
            self.resolve_path("")
        } else {
            self.resolve_path(path)
        };

        if mount.is_empty() {
            // Root listing — return mount names as directories (size 0)
            let names: Vec<(String, u64)> = self
                .mounts
                .iter()
                .map(|m| (format!("{}/", m.name), 0))
                .collect();
            return Ok(names);
        }

        if let Some(m) = self.mounts.iter_mut().find(|m| m.name == mount) {
            Ok(m.fs.list())
        } else {
            Err("no such directory")
        }
    }

    pub fn read_file(&mut self, path: &str) -> Option<Vec<u8>> {
        let (mount, file) = self.resolve_path(path);
        if file.is_empty() {
            return None;
        }
        let m = self.find_mount_mut(&mount)?;
        m.fs.read_file(&file)
    }

    pub fn write_file(&mut self, path: &str, data: &[u8]) -> bool {
        let (mount, file) = self.resolve_path(path);
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
        let (mount, file) = self.resolve_path(path);
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
