// File descriptor table for userland file I/O.
//
// Files are loaded entirely into memory on open and written back to VFS on close.

use alloc::string::String;
use alloc::vec::Vec;

use crate::vfs::Vfs;

const O_READ: u64 = 1;
const O_WRITE: u64 = 2;
const O_CREATE: u64 = 4;
const O_TRUNCATE: u64 = 8;

struct OpenFile {
    path: String,
    data: Vec<u8>,
    position: usize,
    writable: bool,
    modified: bool,
}

static mut FD_TABLE: Vec<Option<OpenFile>> = Vec::new();

fn fd_table() -> &'static mut Vec<Option<OpenFile>> {
    unsafe { &mut *(&raw mut FD_TABLE) }
}

/// Open a file. Returns fd index, or u64::MAX on error.
pub fn open(vfs: &mut Vfs, path: &str, flags: u64) -> u64 {
    let writable = flags & O_WRITE != 0;
    let create = flags & O_CREATE != 0;
    let truncate = flags & O_TRUNCATE != 0;

    let data = if truncate && create {
        // File::create() — start with empty data
        Vec::new()
    } else {
        match vfs.read_file(path) {
            Some(data) => data,
            None => {
                if create {
                    Vec::new()
                } else {
                    return u64::MAX; // file not found
                }
            }
        }
    };

    let file = OpenFile {
        path: String::from(path),
        data,
        position: 0,
        writable,
        modified: false,
    };

    let table = fd_table();
    // Find a free slot
    for (i, slot) in table.iter_mut().enumerate() {
        if slot.is_none() {
            *slot = Some(file);
            return i as u64;
        }
    }
    // No free slot, push new
    let fd = table.len();
    table.push(Some(file));
    fd as u64
}

/// Close a file descriptor. Writes back to VFS if modified.
pub fn close(vfs: &mut Vfs, fd: u64) -> u64 {
    let table = fd_table();
    let fd = fd as usize;
    if fd >= table.len() {
        return u64::MAX;
    }
    if let Some(file) = table[fd].take() {
        if file.modified && file.writable {
            vfs.write_file(&file.path, &file.data);
        }
    }
    0
}

/// Read from a file descriptor.
pub fn read(fd: u64, buf: &mut [u8]) -> u64 {
    let table = fd_table();
    let fd = fd as usize;
    if fd >= table.len() {
        return u64::MAX;
    }
    let Some(file) = table[fd].as_mut() else {
        return u64::MAX;
    };
    let available = file.data.len().saturating_sub(file.position);
    let count = buf.len().min(available);
    buf[..count].copy_from_slice(&file.data[file.position..file.position + count]);
    file.position += count;
    count as u64
}

/// Write to a file descriptor.
pub fn write(fd: u64, buf: &[u8]) -> u64 {
    let table = fd_table();
    let fd = fd as usize;
    if fd >= table.len() {
        return u64::MAX;
    }
    let Some(file) = table[fd].as_mut() else {
        return u64::MAX;
    };
    if !file.writable {
        return u64::MAX;
    }

    let end = file.position + buf.len();
    if end > file.data.len() {
        file.data.resize(end, 0);
    }
    file.data[file.position..end].copy_from_slice(buf);
    file.position = end;
    file.modified = true;
    buf.len() as u64
}

/// Seek in a file descriptor.
/// whence: 0=Start, 1=Current, 2=End
pub fn seek(fd: u64, offset: i64, whence: u64) -> u64 {
    let table = fd_table();
    let fd = fd as usize;
    if fd >= table.len() {
        return u64::MAX;
    }
    let Some(file) = table[fd].as_mut() else {
        return u64::MAX;
    };

    let new_pos = match whence {
        0 => offset as usize,                                    // SEEK_SET
        1 => (file.position as i64 + offset) as usize,          // SEEK_CUR
        2 => (file.data.len() as i64 + offset) as usize,        // SEEK_END
        _ => return u64::MAX,
    };

    file.position = new_pos.min(file.data.len());
    file.position as u64
}

/// Get file metadata. Returns (file_type << 32) | size.
/// file_type: 1 = regular file
pub fn fstat(fd: u64) -> u64 {
    let table = fd_table();
    let fd = fd as usize;
    if fd >= table.len() {
        return u64::MAX;
    }
    let Some(file) = table[fd].as_ref() else {
        return u64::MAX;
    };
    let file_type: u64 = 1; // regular file
    let size = file.data.len() as u64;
    (file_type << 32) | size
}

/// Flush a file descriptor (write data back to VFS without closing).
pub fn fsync(vfs: &mut Vfs, fd: u64) -> u64 {
    let table = fd_table();
    let fd = fd as usize;
    if fd >= table.len() {
        return u64::MAX;
    }
    let Some(file) = table[fd].as_mut() else {
        return u64::MAX;
    };
    if file.modified && file.writable {
        vfs.write_file(&file.path, &file.data);
        file.modified = false;
    }
    0
}

/// Close all open file descriptors (called on process exit).
pub fn close_all(vfs: &mut Vfs) {
    let table = fd_table();
    for slot in table.iter_mut() {
        if let Some(file) = slot.take() {
            if file.modified && file.writable {
                vfs.write_file(&file.path, &file.data);
            }
        }
    }
}
