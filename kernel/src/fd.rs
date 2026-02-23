use alloc::string::String;
use alloc::vec::Vec;

use crate::vfs::Vfs;
use crate::{keyboard, log, pipe};
use crate::drivers::serial;

const O_WRITE: u64 = 2;
const O_CREATE: u64 = 4;
const O_TRUNCATE: u64 = 8;

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

pub type FdTable = Vec<Option<Descriptor>>;

pub fn alloc(table: &mut FdTable, desc: Descriptor) -> u64 {
    if let Some(i) = table.iter().position(|slot| slot.is_none()) {
        table[i] = Some(desc);
        i as u64
    } else {
        let i = table.len();
        table.push(Some(desc));
        i as u64
    }
}

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

pub fn close(table: &mut FdTable, vfs: &mut Vfs, fd: u64) -> u64 {
    let Some(desc) = table.get_mut(fd as usize).and_then(|slot| slot.take()) else {
        return u64::MAX;
    };
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
    0
}

pub fn try_read(table: &mut FdTable, fd: u64, buf: &mut [u8]) -> Option<u64> {
    let desc = table.get_mut(fd as usize)?.as_mut()?;
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
                None
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

pub fn try_write(table: &mut FdTable, fd: u64, buf: &[u8]) -> Option<u64> {
    let desc = table.get_mut(fd as usize)?.as_mut()?;
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
                Some(usize::MAX) => Some(u64::MAX),
                Some(n) => Some(n as u64),
                None => None,
            }
        }
        Descriptor::SerialConsole => {
            serial_write_plain(buf);
            Some(buf.len() as u64)
        }
        Descriptor::Keyboard | Descriptor::PipeRead(_) | Descriptor::Framebuffer(_) => Some(u64::MAX),
    }
}

pub fn seek(table: &mut FdTable, fd: u64, offset: i64, whence: u64) -> u64 {
    let Some(Descriptor::File(file)) = table.get_mut(fd as usize).and_then(|s| s.as_mut()) else {
        return u64::MAX;
    };
    let new_pos = match whence {
        0 => offset as usize,
        1 => (file.position as i64 + offset) as usize,
        2 => (file.data.len() as i64 + offset) as usize,
        _ => return u64::MAX,
    };
    file.position = new_pos.min(file.data.len());
    file.position as u64
}

/// Returns (type << 32) | payload. Types: 1=file, 2=pipe, 3=keyboard, 4=serial, 5=framebuffer.
/// Payload: file size for files, 0 otherwise. Returns 0 for invalid FD.
pub fn fstat(table: &mut FdTable, fd: u64) -> u64 {
    match table.get(fd as usize).and_then(|s| s.as_ref()) {
        Some(Descriptor::File(file)) => (1u64 << 32) | file.data.len() as u64,
        Some(Descriptor::PipeRead(_) | Descriptor::PipeWrite(_)) => 2u64 << 32,
        Some(Descriptor::Keyboard) => 3u64 << 32,
        Some(Descriptor::SerialConsole) => 4u64 << 32,
        Some(Descriptor::Framebuffer(_)) => 5u64 << 32,
        None => 0,
    }
}

pub fn fsync(table: &mut FdTable, vfs: &mut Vfs, fd: u64) -> u64 {
    let Some(Descriptor::File(file)) = table.get_mut(fd as usize).and_then(|s| s.as_mut()) else {
        return u64::MAX;
    };
    if file.modified && file.writable {
        if !vfs.write_file(&file.path, &file.data) {
            return u64::MAX;
        }
        file.modified = false;
    }
    0
}

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

pub fn has_data(table: &FdTable, fd: u64) -> bool {
    match table.get(fd as usize).and_then(|s| s.as_ref()) {
        Some(Descriptor::PipeRead(id)) => pipe::has_data(*id),
        Some(Descriptor::Keyboard) => keyboard::has_data(),
        Some(Descriptor::File(_)) | Some(Descriptor::Framebuffer(_)) => true,
        _ => false,
    }
}

/// Strips ANSI escape sequences before writing to serial.
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
