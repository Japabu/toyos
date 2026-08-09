//! An open file.
//!
//! **Two handles to one `FileObject` share the cursor.** That is a change from
//! the descriptor table, where `dup` cloned the `OpenFile` and the two moved
//! apart; it is what an object model means, and it is POSIX's answer for `dup`
//! as well. A caller that wants an independent cursor opens the path again.

use alloc::string::String;
use alloc::sync::Arc;

use crate::file_cache::{self, FileId};
use crate::sync::Lock;

use super::{KObjectVariant, ObjectCore};

pub struct OpenFileState {
    pub path: String,
    pub file_id: FileId,
    pub position: usize,
    pub modified: bool,
    pub mtime: u64,
}

/// **This takes the VFS lock** — flushing needs it — which is why nothing in
/// the handle layer accepts a `&mut Vfs`: a caller still holding that guard
/// when the last reference to a file drops would deadlock against itself.
impl Drop for OpenFileState {
    fn drop(&mut self) {
        let mut vfs = crate::vfs::lock();
        if self.modified {
            if let Err(e) = vfs.flush_file(&self.path, self.file_id, self.mtime) {
                crate::log!("warning: flush failed on close: {}: {}", self.path, e);
            }
        }
        if file_cache::release(self.file_id) {
            vfs.close_file(&self.path, self.file_id);
        }
    }
}

pub struct FileObject {
    pub(super) core: ObjectCore,
    state: Lock<OpenFileState>,
}

impl FileObject {
    pub fn new(state: OpenFileState) -> Arc<Self> {
        Arc::new(Self { core: Self::new_core(), state: Lock::new(state) })
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut OpenFileState) -> R) -> R {
        f(&mut self.state.lock())
    }
}
