// Per-process file descriptor table.
//
// Each descriptor is either a VFS-backed file (loaded into memory on open,
// written back on close), a pipe endpoint, the keyboard, or the serial console.

use alloc::string::String;
use alloc::vec::Vec;

use crate::vfs::Vfs;
use crate::{keyboard, log, pipe};
use crate::drivers::serial;

const O_WRITE: u64 = 2;
const O_CREATE: u64 = 4;
const O_TRUNCATE: u64 = 8;

pub const MAX_FDS: usize = 32;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct FramebufferInfo {
    pub addr: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub pixel_format: u32,
}

pub struct OpenFile {
    path: String,
    data: Vec<u8>,
    position: usize,
    writable: bool,
    modified: bool,
}

pub enum Descriptor {
    File(OpenFile),
    PipeRead(usize),
    PipeWrite(usize),
    Keyboard,
    SerialConsole,
    Framebuffer(FramebufferInfo),
}

pub type FdTable = [Option<Descriptor>; MAX_FDS];

pub fn new_fd_table() -> FdTable {
    [const { None }; MAX_FDS]
}

/// Allocate a descriptor in the given FD table. Returns fd index or `u64::MAX`.
pub fn alloc(table: &mut FdTable, desc: Descriptor) -> u64 {
    for (i, slot) in table.iter_mut().enumerate() {
        if slot.is_none() {
            *slot = Some(desc);
            return i as u64;
        }
    }
    u64::MAX
}

/// Open a VFS file and allocate an FD. Returns fd index or `u64::MAX`.
pub fn open(table: &mut FdTable, vfs: &mut Vfs, path: &str, flags: u64) -> u64 {
    let writable = flags & O_WRITE != 0;
    let create = flags & O_CREATE != 0;
    let truncate = flags & O_TRUNCATE != 0;

    let data = if truncate && create {
        Vec::new()
    } else {
        match vfs.read_file(path) {
            Some(data) => data,
            None => {
                if create {
                    Vec::new()
                } else {
                    return u64::MAX;
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

    alloc(table, Descriptor::File(file))
}

/// Close a file descriptor.
pub fn close(table: &mut FdTable, vfs: &mut Vfs, fd: u64) -> u64 {
    let fd = fd as usize;
    if fd >= MAX_FDS {
        return u64::MAX;
    }
    if let Some(desc) = table[fd].take() {
        match desc {
            Descriptor::File(file) => {
                if file.modified && file.writable {
                    if !vfs.write_file(&file.path, &file.data) {
                        return u64::MAX;
                    }
                }
            }
            Descriptor::PipeRead(id) => pipe::close_read(id),
            Descriptor::PipeWrite(id) => pipe::close_write(id),
            Descriptor::Keyboard | Descriptor::SerialConsole | Descriptor::Framebuffer(_) => {}
        }
    }
    0
}

/// Read from a file descriptor. Returns bytes read or `u64::MAX` on error.
/// Returns `None` if the read would block (caller should context-switch).
pub fn try_read(table: &mut FdTable, fd: u64, buf: &mut [u8]) -> Option<u64> {
    let fd = fd as usize;
    if fd >= MAX_FDS {
        return Some(u64::MAX);
    }
    let desc = table[fd].as_mut()?;
    match desc {
        Descriptor::File(file) => {
            let available = file.data.len().saturating_sub(file.position);
            let count = buf.len().min(available);
            buf[..count].copy_from_slice(&file.data[file.position..file.position + count]);
            file.position += count;
            Some(count as u64)
        }
        Descriptor::PipeRead(id) => {
            pipe::try_read(*id, buf).map(|n| n as u64)
        }
        Descriptor::Keyboard => {
            // Non-blocking: return first available byte(s), or None to block
            crate::drivers::xhci::poll_global();
            if let Some(ch) = keyboard::try_read_char() {
                buf[0] = ch;
                let mut count = 1usize;
                while count < buf.len() {
                    if let Some(ch) = keyboard::try_read_char() {
                        buf[count] = ch;
                        count += 1;
                    } else {
                        break;
                    }
                }
                Some(count as u64)
            } else {
                None // would block
            }
        }
        Descriptor::Framebuffer(info) => {
            let info_bytes = unsafe {
                core::slice::from_raw_parts(
                    info as *const FramebufferInfo as *const u8,
                    core::mem::size_of::<FramebufferInfo>(),
                )
            };
            let count = buf.len().min(info_bytes.len());
            buf[..count].copy_from_slice(&info_bytes[..count]);
            Some(count as u64)
        }
        Descriptor::PipeWrite(_) | Descriptor::SerialConsole => Some(u64::MAX),
    }
}

/// Write to a file descriptor. Returns bytes written or `u64::MAX` on error.
/// Returns `None` if the write would block.
pub fn try_write(table: &mut FdTable, fd: u64, buf: &[u8]) -> Option<u64> {
    let fd = fd as usize;
    if fd >= MAX_FDS {
        return Some(u64::MAX);
    }
    let desc = table[fd].as_mut()?;
    match desc {
        Descriptor::File(file) => {
            if !file.writable {
                return Some(u64::MAX);
            }
            let end = file.position + buf.len();
            if end > file.data.len() {
                file.data.resize(end, 0);
            }
            file.data[file.position..end].copy_from_slice(buf);
            file.position = end;
            file.modified = true;
            Some(buf.len() as u64)
        }
        Descriptor::PipeWrite(id) => {
            match pipe::try_write(*id, buf) {
                Some(usize::MAX) => Some(u64::MAX), // broken pipe
                Some(n) => Some(n as u64),
                None => None, // would block
            }
        }
        Descriptor::SerialConsole => {
            serial_write_plain(buf);
            Some(buf.len() as u64)
        }
        Descriptor::Keyboard | Descriptor::PipeRead(_) | Descriptor::Framebuffer(_) => Some(u64::MAX),
    }
}

/// Seek in a file descriptor.
pub fn seek(table: &mut FdTable, fd: u64, offset: i64, whence: u64) -> u64 {
    let fd = fd as usize;
    if fd >= MAX_FDS {
        return u64::MAX;
    }
    let Some(Descriptor::File(file)) = table[fd].as_mut() else { return u64::MAX };

    let new_pos = match whence {
        0 => offset as usize,
        1 => (file.position as i64 + offset) as usize,
        2 => (file.data.len() as i64 + offset) as usize,
        _ => return u64::MAX,
    };

    file.position = new_pos.min(file.data.len());
    file.position as u64
}

/// Get file metadata. Returns (file_type << 32) | size.
pub fn fstat(table: &mut FdTable, fd: u64) -> u64 {
    let fd = fd as usize;
    if fd >= MAX_FDS {
        return u64::MAX;
    }
    let Some(Descriptor::File(file)) = table[fd].as_mut() else { return u64::MAX };
    let file_type: u64 = 1;
    let size = file.data.len() as u64;
    (file_type << 32) | size
}

/// Flush a file descriptor.
pub fn fsync(table: &mut FdTable, vfs: &mut Vfs, fd: u64) -> u64 {
    let fd = fd as usize;
    if fd >= MAX_FDS {
        return u64::MAX;
    }
    let Some(Descriptor::File(file)) = table[fd].as_mut() else { return u64::MAX };
    if file.modified && file.writable {
        if !vfs.write_file(&file.path, &file.data) {
            return u64::MAX;
        }
        file.modified = false;
    }
    0
}

/// Close all open file descriptors.
pub fn close_all(table: &mut FdTable, vfs: &mut Vfs) {
    for slot in table.iter_mut() {
        if let Some(desc) = slot.take() {
            match desc {
                Descriptor::File(file) => {
                    if file.modified && file.writable {
                        if !vfs.write_file(&file.path, &file.data) {
                            log!("warning: VFS write failed on process exit: {}", file.path);
                        }
                    }
                }
                Descriptor::PipeRead(id) => pipe::close_read(id),
                Descriptor::PipeWrite(id) => pipe::close_write(id),
                Descriptor::Keyboard | Descriptor::SerialConsole | Descriptor::Framebuffer(_) => {}
            }
        }
    }
}

/// Check if an FD has data available for reading (for poll).
pub fn has_data(table: &FdTable, fd: u64) -> bool {
    let fd = fd as usize;
    if fd >= MAX_FDS {
        return false;
    }
    match &table[fd] {
        Some(Descriptor::PipeRead(id)) => pipe::has_data(*id),
        Some(Descriptor::Keyboard) => keyboard::has_data(),
        Some(Descriptor::File(_)) | Some(Descriptor::Framebuffer(_)) => true,
        _ => false,
    }
}

/// Write bytes to serial, skipping ANSI escape sequences.
fn serial_write_plain(bytes: &[u8]) {
    let mut i = 0;
    let mut start = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1B && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            if start < i { serial::write_bytes(&bytes[start..i]); }
            i += 2;
            while i < bytes.len() && !(0x40..=0x7E).contains(&bytes[i]) {
                i += 1;
            }
            if i < bytes.len() { i += 1; }
            start = i;
        } else {
            i += 1;
        }
    }
    if start < bytes.len() { serial::write_bytes(&bytes[start..]); }
}
