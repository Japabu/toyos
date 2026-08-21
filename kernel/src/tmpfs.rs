use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::file_backing::FileBacking;
use crate::file_cache::{self, FileId};
use toyos_abi::syscall::SyscallError;

use crate::vfs::FileSystem;

/// Reads a tmpfs file for the ELF loader, which demand-pages every executable
/// through a `FileBacking` and had no way to reach a mount whose pages are its
/// only storage. Without this nothing under /tmp was spawnable or dlopenable.
///
/// `copy_page_out`, not `file_cache::read_page`: a tmpfs page *is* the file, so
/// there is no miss for a backing to satisfy — and the miss path is what calls
/// a backing, so reading through it here would recurse.
struct TmpfsBacking {
    file_id: FileId,
}

impl FileBacking for TmpfsBacking {
    /// Never `Err`: the pages are the file, so there is no device to refuse.
    fn read_page(&self, file_offset: u64, buf: &mut [u8; 4096]) -> crate::block::BlockResult {
        // An absent page below the file size is a hole a seek-and-write left,
        // and a hole reads as zeros.
        if file_offset >= file_cache::size(self.file_id)
            || !file_cache::copy_page_out(self.file_id, (file_offset / 4096) as u32, buf)
        {
            buf.fill(0);
        }
        Ok(())
    }

    fn file_size(&self) -> u64 {
        file_cache::size(self.file_id)
    }
}

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
    /// The one implementation that can honour the limit before it allocates,
    /// and the one that needs to: nothing caps how many files a process may
    /// create here.
    fn list(&mut self, limit: usize) -> Result<Vec<(String, u64)>, SyscallError> {
        if self.files.len() > limit {
            return Err(SyscallError::ResourceExhausted);
        }
        Ok(self.files.iter().map(|(name, (file_id, _))| {
            (name.clone(), file_cache::size(*file_id))
        }).collect())
    }

    /// Never `Err`. There is no device under this mount to refuse, which is the
    /// same reason `TmpfsBacking::read_page` cannot fail.
    fn file_mtime(&mut self, name: &str) -> Result<u64, SyscallError> {
        self.files.get(name).map(|(_, mtime)| *mtime).ok_or(SyscallError::NotFound)
    }

    fn read_link(&mut self, name: &str) -> Result<Option<String>, SyscallError> {
        Ok(self.symlinks.get(name).cloned())
    }

    fn open_file(&mut self, name: &str) -> Result<(FileId, Option<Arc<dyn FileBacking>>), SyscallError> {
        let (file_id, _) = self.files.get(name).ok_or(SyscallError::NotFound)?;
        file_cache::open(*file_id);
        Ok((*file_id, None)) // tmpfs: no backing, data is in the file cache
    }

    fn create(&mut self, name: &str, mtime: u64) -> Result<FileId, SyscallError> {
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

    fn delete(&mut self, name: &str) -> Result<(), SyscallError> {
        if let Some((file_id, _)) = self.files.remove(name) {
            let _ = file_cache::mark_deleted(file_id);
            return Ok(());
        }
        if self.symlinks.remove(name).is_some() {
            return Ok(());
        }
        Err(SyscallError::NotFound)
    }

    fn rename(&mut self, old: &str, new: &str) -> Result<(), SyscallError> {
        if let Some((target_id, _)) = self.files.remove(new) {
            let _ = file_cache::mark_deleted(target_id);
        }
        if let Some(entry) = self.files.remove(old) {
            self.files.insert(String::from(new), entry);
            Ok(())
        } else if let Some(target) = self.symlinks.remove(old) {
            self.symlinks.insert(String::from(new), target);
            Ok(())
        } else {
            Err(SyscallError::NotFound)
        }
    }

    fn write_page(&mut self, _file_id: FileId, _page_idx: u32, _data: &[u8; 4096]) -> Result<(), SyscallError> {
        Ok(()) // tmpfs: data is already in the file cache (canonical storage)
    }

    fn update_metadata(&mut self, file_id: FileId, _size: u64, mtime: u64) -> Result<(), SyscallError> {
        for (fid, mt) in self.files.values_mut() {
            if *fid == file_id {
                *mt = mtime;
                return Ok(());
            }
        }
        Ok(())
    }

    fn create_symlink(&mut self, name: &str, target: &str) -> Result<(), SyscallError> {
        self.symlinks.insert(String::from(name), String::from(target));
        Ok(())
    }

    fn sync(&mut self) -> Result<(), SyscallError> {
        Ok(())
    }

    fn open_backing(&mut self, name: &str) -> Result<Arc<dyn FileBacking>, SyscallError> {
        let (file_id, _) = self.files.get(name).ok_or(SyscallError::NotFound)?;
        Ok(Arc::new(TmpfsBacking { file_id: *file_id }))
    }
}
