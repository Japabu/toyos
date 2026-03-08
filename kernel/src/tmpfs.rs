use alloc::borrow::Cow;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use crate::vfs::FileSystem;

/// In-memory filesystem for /tmp and similar ephemeral mounts.
pub struct TmpFs {
    files: BTreeMap<String, (Vec<u8>, u64)>,
    symlinks: BTreeMap<String, String>,
}

impl TmpFs {
    pub fn new() -> Self {
        Self { files: BTreeMap::new(), symlinks: BTreeMap::new() }
    }
}

impl FileSystem for TmpFs {
    fn list(&mut self) -> Vec<(String, u64)> {
        self.files.iter().map(|(name, (data, _))| (name.clone(), data.len() as u64)).collect()
    }

    fn read_file(&mut self, name: &str) -> Result<Cow<'static, [u8]>, &'static str> {
        match self.files.get(name) {
            Some((data, _)) => Ok(Cow::Owned(data.clone())),
            None => Err("not found"),
        }
    }

    fn read_link(&mut self, name: &str) -> Option<String> {
        self.symlinks.get(name).cloned()
    }

    fn file_mtime(&mut self, name: &str) -> u64 {
        self.files.get(name).map_or(0, |(_, mtime)| *mtime)
    }

    fn create(&mut self, name: &str, data: &[u8], mtime: u64) -> Result<(), &'static str> {
        self.files.insert(String::from(name), (Vec::from(data), mtime));
        Ok(())
    }

    fn delete(&mut self, name: &str) -> bool {
        self.symlinks.remove(name).is_some() || self.files.remove(name).is_some()
    }

    fn delete_prefix(&mut self, prefix: &str) {
        self.files.retain(|k, _| !k.starts_with(prefix));
        self.symlinks.retain(|k, _| !k.starts_with(prefix));
    }

    fn create_symlink(&mut self, name: &str, target: &str) -> Result<(), &'static str> {
        self.symlinks.insert(String::from(name), String::from(target));
        Ok(())
    }

    fn sync(&mut self) {}
}
