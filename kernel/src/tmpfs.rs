use alloc::borrow::Cow;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use crate::vfs::FileSystem;

/// In-memory filesystem for /tmp and similar ephemeral mounts.
pub struct TmpFs {
    files: BTreeMap<String, (Vec<u8>, u64)>,
}

impl TmpFs {
    pub fn new() -> Self {
        Self { files: BTreeMap::new() }
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

    fn read_link(&mut self, _name: &str) -> Option<String> {
        None
    }

    fn file_mtime(&mut self, name: &str) -> u64 {
        self.files.get(name).map_or(0, |(_, mtime)| *mtime)
    }

    fn create(&mut self, name: &str, data: &[u8], mtime: u64) -> bool {
        self.files.insert(String::from(name), (Vec::from(data), mtime));
        true
    }

    fn delete(&mut self, name: &str) -> bool {
        self.files.remove(name).is_some()
    }
}
