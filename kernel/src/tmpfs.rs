use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use crate::file_cache::{self, FileId};
use crate::vfs::FileSystem;

/// In-memory filesystem. File data lives in the unified file cache
/// (non-evictable pages). tmpfs only stores the namespace mapping.
pub struct TmpFs {
    /// name → (FileId, mtime)
    files: BTreeMap<String, (FileId, u64)>,
    symlinks: BTreeMap<String, String>,
}

impl TmpFs {
    pub fn new() -> Self {
        Self { files: BTreeMap::new(), symlinks: BTreeMap::new() }
    }
}

impl FileSystem for TmpFs {
    fn list(&mut self) -> Vec<(String, u64)> {
        self.files.iter().map(|(name, (file_id, _))| {
            (name.clone(), file_cache::size(*file_id))
        }).collect()
    }

    fn file_size(&mut self, name: &str) -> Option<u64> {
        let (file_id, _) = self.files.get(name)?;
        Some(file_cache::size(*file_id))
    }

    fn file_mtime(&mut self, name: &str) -> u64 {
        self.files.get(name).map_or(0, |(_, mtime)| *mtime)
    }

    fn read_link(&mut self, name: &str) -> Option<String> {
        self.symlinks.get(name).cloned()
    }

    fn open_file(&mut self, name: &str) -> Option<(FileId, Option<alloc::sync::Arc<dyn crate::file_backing::FileBacking>>)> {
        let (file_id, _) = self.files.get(name)?;
        file_cache::open(*file_id);
        Some((*file_id, None)) // tmpfs: no backing, data is in the file cache
    }

    fn create(&mut self, name: &str, mtime: u64) -> Result<FileId, &'static str> {
        // If file already exists, return its existing FileId
        if let Some((file_id, _)) = self.files.get(name) {
            return Ok(*file_id);
        }
        let file_id = file_cache::create_file(false); // non-evictable
        self.files.insert(String::from(name), (file_id, mtime));
        Ok(file_id)
    }

    fn close_file(&mut self, _file_id: FileId) {
        // tmpfs: no-op. Pages persist in file cache (non-evictable).
    }

    fn delete(&mut self, name: &str) -> bool {
        if let Some((file_id, _)) = self.files.remove(name) {
            file_cache::mark_deleted(file_id);
            return true;
        }
        self.symlinks.remove(name).is_some()
    }

    fn delete_prefix(&mut self, prefix: &str) {
        let to_delete: Vec<String> = self.files.keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect();
        for name in to_delete {
            if let Some((file_id, _)) = self.files.remove(&name) {
                file_cache::mark_deleted(file_id);
            }
        }
        self.symlinks.retain(|k, _| !k.starts_with(prefix));
    }

    fn rename(&mut self, old: &str, new: &str) -> Result<(), &'static str> {
        // Handle target: if new name exists, unlink it
        if let Some((target_id, _)) = self.files.remove(new) {
            file_cache::mark_deleted(target_id);
        }
        // Re-key the source entry
        if let Some(entry) = self.files.remove(old) {
            self.files.insert(String::from(new), entry);
            Ok(())
        } else if let Some(target) = self.symlinks.remove(old) {
            self.symlinks.insert(String::from(new), target);
            Ok(())
        } else {
            Err("not found")
        }
    }

    fn write_page(&mut self, _file_id: FileId, _page_idx: u32, _data: &[u8; 4096]) -> Result<(), &'static str> {
        Ok(()) // tmpfs: data is already in the file cache (canonical storage)
    }

    fn update_metadata(&mut self, file_id: FileId, _size: u64, mtime: u64) -> Result<(), &'static str> {
        // Find and update mtime for this FileId
        for (_, (fid, mt)) in self.files.iter_mut() {
            if *fid == file_id {
                *mt = mtime;
                return Ok(());
            }
        }
        Ok(())
    }

    fn create_symlink(&mut self, name: &str, target: &str) -> Result<(), &'static str> {
        self.symlinks.insert(String::from(name), String::from(target));
        Ok(())
    }

    fn sync(&mut self) {}
}
