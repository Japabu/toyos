use alloc::string::String;
use alloc::vec::Vec;

use crate::id_map::IdMap;
use crate::vfs::Vfs;
use crate::{device, keyboard, mouse, log, pipe};
use crate::drivers::serial;
use toyos_abi::syscall::{FileType, SyscallError};

const O_WRITE: u64 = 2;
const O_CREATE: u64 = 4;
const O_TRUNCATE: u64 = 8;


#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct FramebufferInfo {
    pub token: [u32; 2],
    pub cursor_token: u32,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub pixel_format: u32,
    pub flags: u32,
}

#[derive(Clone)]
pub struct OpenFile {
    path: String,
    data: Vec<u8>,
    position: usize,
    writable: bool,
    modified: bool,
    mtime: u64,
}

pub enum Descriptor {
    File(OpenFile),
    PipeRead(usize),
    PipeWrite(usize),
    TtyRead(usize),
    TtyWrite(usize),
    Keyboard,
    Mouse,
    SerialConsole,
    Framebuffer(FramebufferInfo),
}

pub type FdTable = IdMap<u64, Descriptor>;

/// Duplicate a descriptor, bumping pipe refcounts as needed.
pub fn dup(desc: &Descriptor) -> Descriptor {
    match desc {
        Descriptor::PipeRead(id) => { pipe::add_reader(*id); Descriptor::PipeRead(*id) }
        Descriptor::PipeWrite(id) => { pipe::add_writer(*id); Descriptor::PipeWrite(*id) }
        Descriptor::TtyRead(id) => { pipe::add_reader(*id); Descriptor::TtyRead(*id) }
        Descriptor::TtyWrite(id) => { pipe::add_writer(*id); Descriptor::TtyWrite(*id) }
        Descriptor::File(file) => Descriptor::File(file.clone()),
        Descriptor::Keyboard => Descriptor::Keyboard,
        Descriptor::Mouse => Descriptor::Mouse,
        Descriptor::SerialConsole => Descriptor::SerialConsole,
        Descriptor::Framebuffer(info) => Descriptor::Framebuffer(*info),
    }
}

pub fn alloc(table: &mut FdTable, desc: Descriptor) -> u64 {
    table.insert(desc)
}

pub fn open(table: &mut FdTable, vfs: &mut Vfs, path: &str, flags: u64) -> u64 {
    let writable = flags & O_WRITE != 0;
    let create = flags & O_CREATE != 0;
    let truncate = flags & O_TRUNCATE != 0;

    // Validate path resolves to a real mount + filename
    if create {
        let (_, file) = vfs.resolve_path("/", path);
        if file.is_empty() {
            return SyscallError::InvalidArgument.to_u64();
        }
    }

    let (data, mtime) = if truncate && create {
        let mtime = crate::clock::nanos_since_boot();
        vfs.write_file(path, &[], mtime);
        (Vec::new(), mtime)
    } else {
        match vfs.read_file(path) {
            Ok(data) => {
                let mtime = vfs.file_mtime(path);
                (data.into_owned(), mtime)
            }
            Err(_) => {
                if create {
                    let mtime = crate::clock::nanos_since_boot();
                    vfs.write_file(path, &[], mtime);
                    (Vec::new(), mtime)
                } else {
                    return SyscallError::NotFound.to_u64();
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
        mtime,
    };

    alloc(table, Descriptor::File(file))
}

pub fn close(table: &mut FdTable, vfs: &mut Vfs, fd: u64, pid: u32) -> u64 {
    let Some(desc) = table.remove(fd) else {
        return SyscallError::NotFound.to_u64();
    };
    match &desc {
        Descriptor::File(file) => {
            if file.modified && file.writable {
                if !vfs.write_file(&file.path, &file.data, file.mtime) {
                    return SyscallError::Unknown.to_u64();
                }
            }
        }
        Descriptor::PipeRead(id) | Descriptor::TtyRead(id) => pipe::close_read(*id),
        Descriptor::PipeWrite(id) | Descriptor::TtyWrite(id) => pipe::close_write(*id),
        Descriptor::Keyboard | Descriptor::Mouse | Descriptor::Framebuffer(_) => {
            device::release_descriptor(&desc, pid);
        }
        Descriptor::SerialConsole => {}
    }
    0
}

pub fn try_read(table: &mut FdTable, fd: u64, buf: &mut [u8]) -> Option<u64> {
    let desc = table.get_mut(fd)?;
    match desc {
        Descriptor::File(file) => {
            let available = file.data.len().saturating_sub(file.position);
            let count = buf.len().min(available);
            buf[..count].copy_from_slice(&file.data[file.position..file.position + count]);
            file.position += count;
            Some(count as u64)
        }
        Descriptor::PipeRead(id) | Descriptor::TtyRead(id) => {
            pipe::try_read(*id, buf).map(|n| n as u64)
        }
        Descriptor::Keyboard => {
            crate::drivers::xhci::poll_if_pending();
            let event_size = core::mem::size_of::<keyboard::RawKeyEvent>();
            let mut count = 0;
            while count + event_size <= buf.len() {
                if let Some(event) = keyboard::try_read_event() {
                    let bytes = bytemuck::bytes_of(&event);
                    buf[count..count + event_size].copy_from_slice(bytes);
                    count += event_size;
                } else {
                    break;
                }
            }
            if count > 0 { Some(count as u64) } else { None }
        }
        Descriptor::Mouse => {
            crate::drivers::xhci::poll_if_pending();
            let event_size = core::mem::size_of::<mouse::MouseEvent>();
            let mut count = 0;
            while count + event_size <= buf.len() {
                if let Some(event) = mouse::try_read_event() {
                    let bytes = bytemuck::bytes_of(&event);
                    buf[count..count + event_size].copy_from_slice(bytes);
                    count += event_size;
                } else {
                    break;
                }
            }
            if count > 0 { Some(count as u64) } else { None }
        }
        Descriptor::Framebuffer(info) => {
            let bytes = bytemuck::bytes_of(info);
            let count = buf.len().min(bytes.len());
            buf[..count].copy_from_slice(&bytes[..count]);
            Some(count as u64)
        }
        Descriptor::PipeWrite(_) | Descriptor::TtyWrite(_) => Some(SyscallError::PermissionDenied.to_u64()),
        Descriptor::SerialConsole => {
            // Read from serial port (non-blocking: return None if no data)
            let mut count = 0usize;
            while count < buf.len() {
                if let Some(b) = serial::try_read_byte() {
                    buf[count] = b;
                    count += 1;
                    // Return after each line for interactive use
                    if b == b'\n' || b == b'\r' { break; }
                } else if count > 0 {
                    break;
                } else {
                    return None; // No data available, block
                }
            }
            Some(count as u64)
        }
    }
}

pub fn try_write(table: &mut FdTable, fd: u64, buf: &[u8]) -> Option<u64> {
    let desc = table.get_mut(fd)?;
    match desc {
        Descriptor::File(file) => {
            if !file.writable {
                return Some(SyscallError::PermissionDenied.to_u64());
            }
            let end = file.position + buf.len();
            if end > file.data.len() {
                file.data.resize(end, 0);
            }
            file.data[file.position..end].copy_from_slice(buf);
            file.position = end;
            file.modified = true;
            file.mtime = crate::clock::nanos_since_boot();
            Some(buf.len() as u64)
        }
        Descriptor::PipeWrite(id) | Descriptor::TtyWrite(id) => {
            match pipe::try_write(*id, buf) {
                Some(usize::MAX) => Some(SyscallError::NotFound.to_u64()),
                Some(n) => Some(n as u64),
                None => None,
            }
        }
        Descriptor::SerialConsole => {
            serial_write_plain(buf);
            Some(buf.len() as u64)
        }
        Descriptor::Keyboard | Descriptor::Mouse | Descriptor::PipeRead(_) | Descriptor::TtyRead(_) | Descriptor::Framebuffer(_) => Some(SyscallError::PermissionDenied.to_u64()),
    }
}

pub fn seek(table: &mut FdTable, fd: u64, offset: i64, whence: u64) -> u64 {
    let Some(Descriptor::File(file)) = table.get_mut(fd) else {
        return SyscallError::NotFound.to_u64();
    };
    let new_pos = match whence {
        0 => offset,
        1 => (file.position as i64).checked_add(offset).unwrap_or(-1),
        2 => (file.data.len() as i64).checked_add(offset).unwrap_or(-1),
        _ => return SyscallError::InvalidArgument.to_u64(),
    };
    if new_pos < 0 { return SyscallError::InvalidArgument.to_u64(); }
    file.position = (new_pos as usize).min(file.data.len());
    file.position as u64
}

/// Raw stat struct for the syscall boundary (must be Pod for user pointer access).
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Stat {
    pub file_type: u64,
    pub size: u64,
    pub mtime: u64,
}

/// Fill a Stat struct for the given file descriptor. Returns false for invalid FD.
pub fn fstat(table: &FdTable, fd: u64, stat: &mut Stat) -> bool {
    match table.get(fd) {
        Some(Descriptor::File(file)) => {
            stat.file_type = FileType::File as u64;
            stat.size = file.data.len() as u64;
            stat.mtime = file.mtime;
            true
        }
        Some(Descriptor::PipeRead(_) | Descriptor::PipeWrite(_)) => { stat.file_type = FileType::Pipe as u64; true }
        Some(Descriptor::Keyboard) => { stat.file_type = FileType::Keyboard as u64; true }
        Some(Descriptor::Mouse) => { stat.file_type = FileType::Mouse as u64; true }
        Some(Descriptor::SerialConsole) => { stat.file_type = FileType::Serial as u64; true }
        Some(Descriptor::Framebuffer(_)) => { stat.file_type = FileType::Framebuffer as u64; true }
        Some(Descriptor::TtyRead(_) | Descriptor::TtyWrite(_)) => { stat.file_type = FileType::Tty as u64; true }
        None => false,
    }
}

pub fn fsync(table: &mut FdTable, vfs: &mut Vfs, fd: u64) -> u64 {
    let Some(Descriptor::File(file)) = table.get_mut(fd) else {
        return SyscallError::NotFound.to_u64();
    };
    if file.modified && file.writable {
        if !vfs.write_file(&file.path, &file.data, file.mtime) {
            return SyscallError::Unknown.to_u64();
        }
        file.modified = false;
    }
    0
}

pub fn ftruncate(table: &mut FdTable, fd: u64, size: u64) -> u64 {
    let Some(Descriptor::File(file)) = table.get_mut(fd) else {
        return SyscallError::NotFound.to_u64();
    };
    if !file.writable { return SyscallError::PermissionDenied.to_u64(); }
    file.data.resize(size as usize, 0);
    if file.position > size as usize { file.position = size as usize; }
    file.modified = true;
    file.mtime = crate::clock::nanos_since_boot();
    0
}

pub fn close_all(table: &mut FdTable, vfs: &mut Vfs, pid: u32) {
    for (_, desc) in table.drain() {
        match &desc {
            Descriptor::File(file) => {
                if file.modified && file.writable {
                    if !vfs.write_file(&file.path, &file.data, file.mtime) {
                        log!("warning: VFS write failed on process exit: {}", file.path);
                    }
                }
            }
            Descriptor::PipeRead(id) | Descriptor::TtyRead(id) => pipe::close_read(*id),
            Descriptor::PipeWrite(id) | Descriptor::TtyWrite(id) => pipe::close_write(*id),
            Descriptor::Keyboard | Descriptor::Mouse | Descriptor::Framebuffer(_) => {
                device::release_descriptor(&desc, pid);
            }
            Descriptor::SerialConsole => {}
        }
    }
}

pub fn has_data(table: &FdTable, fd: u64) -> bool {
    match table.get(fd) {
        Some(Descriptor::PipeRead(id)) | Some(Descriptor::TtyRead(id)) => pipe::has_data(*id),
        Some(Descriptor::Keyboard) => keyboard::has_data(),
        Some(Descriptor::Mouse) => mouse::has_data(),
        Some(Descriptor::File(_)) | Some(Descriptor::Framebuffer(_)) => true,
        Some(Descriptor::SerialConsole) => serial::has_data(),
        _ => false,
    }
}

pub fn has_space(table: &FdTable, fd: u64) -> bool {
    match table.get(fd) {
        Some(Descriptor::PipeWrite(id)) | Some(Descriptor::TtyWrite(id)) => pipe::has_space(*id),
        Some(Descriptor::File(_)) | Some(Descriptor::SerialConsole) => true,
        _ => false,
    }
}

pub fn mark_tty(table: &mut FdTable, fd: u64) -> u64 {
    let Some(desc) = table.get_mut(fd) else {
        return SyscallError::NotFound.to_u64();
    };
    match desc {
        Descriptor::PipeRead(id) => { *desc = Descriptor::TtyRead(*id); 0 }
        Descriptor::PipeWrite(id) => { *desc = Descriptor::TtyWrite(*id); 0 }
        Descriptor::TtyRead(_) | Descriptor::TtyWrite(_) => 0,
        _ => SyscallError::InvalidArgument.to_u64(),
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
