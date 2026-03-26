use alloc::string::String;
use alloc::sync::Arc;

use crate::file_backing::FileBacking;
use crate::file_cache::{self, FileId};
use crate::id_map::IdMap;
use crate::process::Pid;
use crate::vfs::Vfs;
use crate::{device, keyboard, listener, mouse, pipe, UserAddr};
use crate::pipe::{PipeId, PipeReader, PipeWriter};
use crate::drivers::serial;
pub use toyos_abi::FramebufferInfo;
use toyos_abi::syscall::{FileType, OpenFlags, SeekFrom, SyscallError};

/// Tracks a pipe's mapping into a process's virtual address space.
#[derive(Clone)]
#[allow(dead_code)]
pub struct PipeMapping {
    pub vaddr: UserAddr,
    pub size: u64,
}

pub struct OpenFile {
    path: String,
    file_id: FileId,
    backing: Option<Arc<dyn FileBacking>>,
    position: usize,
    writable: bool,
    modified: bool,
    mtime: u64,
}

impl Clone for OpenFile {
    fn clone(&self) -> Self {
        file_cache::open(self.file_id);
        Self {
            path: self.path.clone(),
            file_id: self.file_id,
            backing: self.backing.clone(),
            position: self.position,
            writable: self.writable,
            modified: false, // cloned fd starts unmodified
            mtime: self.mtime,
        }
    }
}

// ---------------------------------------------------------------------------
// Descriptor
// ---------------------------------------------------------------------------

pub enum Descriptor {
    File(OpenFile),
    PipeRead(PipeReader, Option<PipeMapping>),
    PipeWrite(PipeWriter, Option<PipeMapping>),
    TtyRead(PipeReader),
    TtyWrite(PipeWriter),
    Keyboard,
    Mouse,
    SerialConsole,
    Framebuffer(FramebufferInfo),
    Socket { rx: PipeReader, tx: PipeWriter },
    Nic(crate::net::NicInfo),
    Audio(toyos_abi::audio::AudioInfo),
    Listener(String),
    IoUring(crate::io_uring::RingId),
}

impl Clone for Descriptor {
    fn clone(&self) -> Self {
        match self {
            Self::PipeRead(r, m) => Self::PipeRead(r.clone(), m.clone()),
            Self::PipeWrite(w, m) => Self::PipeWrite(w.clone(), m.clone()),
            Self::TtyRead(r) => Self::TtyRead(r.clone()),
            Self::TtyWrite(w) => Self::TtyWrite(w.clone()),
            Self::Socket { rx, tx } => Self::Socket { rx: rx.clone(), tx: tx.clone() },
            Self::File(file) => Self::File(file.clone()),
            Self::Keyboard => Self::Keyboard,
            Self::Mouse => Self::Mouse,
            Self::SerialConsole => Self::SerialConsole,
            Self::Framebuffer(info) => Self::Framebuffer(*info),
            Self::Nic(info) => Self::Nic(*info),
            Self::Audio(info) => Self::Audio(*info),
            Self::Listener(name) => Self::Listener(name.clone()),
            Self::IoUring(id) => Self::IoUring(*id),
        }
    }
}

impl Descriptor {
    pub fn pipe_id_read(&self) -> Option<PipeId> {
        match self {
            Self::PipeRead(r, _) | Self::TtyRead(r) => Some(r.id()),
            Self::Socket { rx, .. } => Some(rx.id()),
            _ => None,
        }
    }

    pub fn pipe_id_write(&self) -> Option<PipeId> {
        match self {
            Self::PipeWrite(w, _) | Self::TtyWrite(w) => Some(w.id()),
            Self::Socket { tx, .. } => Some(tx.id()),
            _ => None,
        }
    }

    pub fn read_event_source(&self) -> Option<crate::scheduler::EventSource> {
        use crate::scheduler::EventSource;
        match self {
            Self::Keyboard => Some(EventSource::Keyboard),
            Self::Mouse => Some(EventSource::Mouse),
            Self::SerialConsole => Some(EventSource::Keyboard),
            Self::Nic(_) => Some(EventSource::Network),
            Self::Listener(_) => Some(EventSource::Listener),
            Self::PipeRead(r, _) | Self::TtyRead(r) => Some(EventSource::PipeReadable(r.id())),
            Self::Socket { rx, .. } => Some(EventSource::PipeReadable(rx.id())),
            Self::File(_) | Self::Framebuffer(_) | Self::Audio(_) => None,
            Self::PipeWrite(..) | Self::TtyWrite(_) => None,
            Self::IoUring(_) => None,
        }
    }

    pub fn write_event_source(&self) -> Option<crate::scheduler::EventSource> {
        use crate::scheduler::EventSource;
        match self {
            Self::PipeWrite(w, _) | Self::TtyWrite(w) => Some(EventSource::PipeWritable(w.id())),
            Self::Socket { tx, .. } => Some(EventSource::PipeWritable(tx.id())),
            Self::File(_) | Self::SerialConsole => None,
            Self::Keyboard | Self::Mouse | Self::Nic(_) | Self::Audio(_)
            | Self::Framebuffer(_) | Self::Listener(_)
            | Self::PipeRead(..) | Self::TtyRead(_) | Self::IoUring(_) => None,
        }
    }
}

// ---------------------------------------------------------------------------
// FdTable operations
// ---------------------------------------------------------------------------

pub type FdTable = IdMap<u32, Descriptor>;

const MAX_FDS: usize = 1024;

pub fn alloc(table: &mut FdTable, desc: Descriptor) -> Result<u32, SyscallError> {
    if table.len() >= MAX_FDS {
        return Err(SyscallError::ResourceExhausted);
    }
    Ok(table.insert(desc))
}

pub fn alloc_at(table: &mut FdTable, fd_num: u32, desc: Descriptor) {
    table.insert_at(fd_num, desc);
}

pub fn open(table: &mut FdTable, vfs: &mut Vfs, path: &str, flags: OpenFlags) -> u64 {
    let writable = flags.contains(OpenFlags::WRITE);
    let create = flags.contains(OpenFlags::CREATE);
    let truncate = flags.contains(OpenFlags::TRUNCATE);
    let append = flags.contains(OpenFlags::APPEND);

    if create {
        let (_, file) = vfs.resolve_path("/", path);
        if file.is_empty() {
            return SyscallError::InvalidArgument.to_u64();
        }
    }

    // Handle CREATE + TRUNCATE: create (or truncate) empty file
    if truncate && create {
        let mtime = crate::clock::nanos_since_boot();
        // Delete existing file if any
        vfs.delete(path);
        let file_id = match vfs.create_file(path, mtime) {
            Ok(id) => id,
            Err(_) => return SyscallError::Unknown.to_u64(),
        };
        let file = OpenFile {
            path: String::from(path),
            file_id,
            backing: None,
            position: 0,
            writable,
            modified: false,
            mtime,
        };
        return match alloc(table, Descriptor::File(file)) {
            Ok(fd) => fd as u64,
            Err(e) => e.to_u64(),
        };
    }

    // Try to open existing file
    match vfs.open_file(path) {
        Some((file_id, backing)) => {
            let mtime = vfs.file_mtime(path);
            let size = file_cache::size(file_id);
            let position = if append { size as usize } else { 0 };
            let file = OpenFile {
                path: String::from(path),
                file_id,
                backing,
                position,
                writable,
                modified: false,
                mtime,
            };
            match alloc(table, Descriptor::File(file)) {
                Ok(fd) => fd as u64,
                Err(e) => e.to_u64(),
            }
        }
        None => {
            if create {
                let mtime = crate::clock::nanos_since_boot();
                let file_id = match vfs.create_file(path, mtime) {
                    Ok(id) => id,
                    Err(_) => return SyscallError::Unknown.to_u64(),
                };
                let file = OpenFile {
                    path: String::from(path),
                    file_id,
                    backing: None,
                    position: 0,
                    writable,
                    modified: false,
                    mtime,
                };
                match alloc(table, Descriptor::File(file)) {
                    Ok(fd) => fd as u64,
                    Err(e) => e.to_u64(),
                }
            } else {
                SyscallError::NotFound.to_u64()
            }
        }
    }
}

/// Close an fd. Flushes modified files, handles pipe refcounts.
pub fn close(table: &mut FdTable, vfs: &mut Vfs, fd: u32, pid: Pid) -> u64 {
    let Some(desc) = table.remove(fd) else {
        return SyscallError::NotFound.to_u64();
    };
    let sources = [desc.read_event_source(), desc.write_event_source()];
    if sources.iter().any(|s| s.is_some()) {
        crate::io_uring::remove_fd(fd, &sources);
    }
    match &desc {
        Descriptor::File(file) => {
            if file.modified {
                let _ = vfs.flush_file(&file.path, file.file_id, file.mtime);
            }
            let last_ref = file_cache::release(file.file_id);
            if last_ref {
                vfs.close_file(&file.path, file.file_id);
            }
        }
        Descriptor::Keyboard | Descriptor::Mouse | Descriptor::Framebuffer(_) | Descriptor::Nic(_) | Descriptor::Audio(_) => {
            device::release_descriptor(&desc, pid);
        }
        Descriptor::Listener(name) => {
            listener::remove(name);
        }
        Descriptor::IoUring(id) => {
            crate::io_uring::destroy(*id);
        }
        _ => {}
    }
    0
}

pub fn close_all(table: &mut FdTable, vfs: &mut Vfs, pid: Pid) {
    for (_, desc) in table.drain() {
        match &desc {
            Descriptor::File(file) => {
                if file.modified {
                    if let Err(e) = vfs.flush_file(&file.path, file.file_id, file.mtime) {
                        crate::log!("warning: flush failed on process exit: {}: {}", file.path, e);
                    }
                }
                let last_ref = file_cache::release(file.file_id);
                if last_ref {
                    vfs.close_file(&file.path, file.file_id);
                }
            }
            Descriptor::Keyboard | Descriptor::Mouse | Descriptor::Framebuffer(_) | Descriptor::Nic(_) | Descriptor::Audio(_) => {
                device::release_descriptor(&desc, pid);
            }
            Descriptor::Listener(name) => {
                listener::remove(name);
            }
            Descriptor::IoUring(id) => {
                crate::io_uring::destroy(*id);
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Read / Write / Seek / Stat
// ---------------------------------------------------------------------------

pub fn try_read(table: &mut FdTable, fd: u32, buf: &mut [u8]) -> Option<u64> {
    let desc = table.get_mut(fd)?;
    match desc {
        Descriptor::File(file) => {
            let size = file_cache::size(file.file_id) as usize;
            let available = size.saturating_sub(file.position);
            let count = buf.len().min(available);
            if count == 0 {
                return Some(0);
            }
            // Read page by page from file cache
            let mut read = 0;
            while read < count {
                let abs_pos = file.position + read;
                let page_idx = (abs_pos / 4096) as u32;
                let offset_in_page = abs_pos % 4096;
                let remaining_in_page = 4096 - offset_in_page;
                let to_read = remaining_in_page.min(count - read);
                file_cache::read_page(
                    file.file_id,
                    page_idx,
                    offset_in_page,
                    &mut buf[read..read + to_read],
                    file.backing.as_deref(),
                );
                read += to_read;
            }
            file.position += count;
            Some(count as u64)
        }
        Descriptor::PipeRead(r, _) | Descriptor::TtyRead(r) => {
            pipe::try_read(r.id(), buf).map(|n| n as u64)
        }
        Descriptor::Socket { rx, .. } => {
            pipe::try_read(rx.id(), buf).map(|n| n as u64)
        }
        Descriptor::Keyboard => {
            crate::drivers::xhci::poll_if_pending();
            let event_size = core::mem::size_of::<keyboard::RawKeyEvent>();
            let mut count = 0;
            while count + event_size <= buf.len() {
                if let Some(event) = keyboard::try_read_event() {
                    buf[count..count + event_size].copy_from_slice(event.as_bytes());
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
                    buf[count..count + event_size].copy_from_slice(event.as_bytes());
                    count += event_size;
                } else {
                    break;
                }
            }
            if count > 0 { Some(count as u64) } else { None }
        }
        Descriptor::Framebuffer(info) => {
            let bytes = info.as_bytes();
            let count = buf.len().min(bytes.len());
            buf[..count].copy_from_slice(&bytes[..count]);
            Some(count as u64)
        }
        Descriptor::Nic(info) => {
            let bytes = info.as_bytes();
            let count = buf.len().min(bytes.len());
            buf[..count].copy_from_slice(&bytes[..count]);
            Some(count as u64)
        }
        Descriptor::Audio(info) => {
            let bytes = info.as_bytes();
            let count = buf.len().min(bytes.len());
            buf[..count].copy_from_slice(&bytes[..count]);
            Some(count as u64)
        }
        Descriptor::SerialConsole => {
            let mut count = 0usize;
            while count < buf.len() {
                if let Some(b) = serial::try_read_byte() {
                    buf[count] = b;
                    count += 1;
                    if b == b'\n' || b == b'\r' { break; }
                } else if count > 0 {
                    break;
                } else {
                    return None;
                }
            }
            Some(count as u64)
        }
        Descriptor::Listener(_) | Descriptor::PipeWrite(..) | Descriptor::TtyWrite(_)
        | Descriptor::IoUring(_) => {
            Some(SyscallError::PermissionDenied.to_u64())
        }
    }
}

pub fn try_write(table: &mut FdTable, fd: u32, buf: &[u8]) -> Option<u64> {
    let desc = table.get_mut(fd)?;
    match desc {
        Descriptor::File(file) => {
            if !file.writable {
                return Some(SyscallError::PermissionDenied.to_u64());
            }
            // Write page by page to file cache
            let mut written = 0;
            while written < buf.len() {
                let abs_pos = file.position + written;
                let page_idx = (abs_pos / 4096) as u32;
                let offset_in_page = abs_pos % 4096;
                let remaining_in_page = 4096 - offset_in_page;
                let to_write = remaining_in_page.min(buf.len() - written);
                file_cache::write_page(
                    file.file_id,
                    page_idx,
                    offset_in_page,
                    &buf[written..written + to_write],
                    file.backing.as_deref(),
                );
                written += to_write;
            }
            file.position += buf.len();
            file.modified = true;
            file.mtime = crate::clock::nanos_since_boot();
            Some(buf.len() as u64)
        }
        Descriptor::PipeWrite(w, _) | Descriptor::TtyWrite(w) => {
            match pipe::try_write(w.id(), buf) {
                Some(pipe::PipeWrite::BrokenPipe) => Some(SyscallError::NotFound.to_u64()),
                Some(pipe::PipeWrite::Wrote(n)) => Some(n as u64),
                None => None,
            }
        }
        Descriptor::Socket { tx, .. } => {
            match pipe::try_write(tx.id(), buf) {
                Some(pipe::PipeWrite::BrokenPipe) => Some(SyscallError::NotFound.to_u64()),
                Some(pipe::PipeWrite::Wrote(n)) => Some(n as u64),
                None => None,
            }
        }
        Descriptor::SerialConsole => {
            serial::write(buf);
            Some(buf.len() as u64)
        }
        _ => Some(SyscallError::PermissionDenied.to_u64()),
    }
}

pub fn seek(table: &mut FdTable, fd: u32, pos: SeekFrom) -> u64 {
    let Some(Descriptor::File(file)) = table.get_mut(fd) else {
        return SyscallError::NotFound.to_u64();
    };
    let size = file_cache::size(file.file_id) as usize;
    let new_pos = match pos {
        SeekFrom::Start(n) => n as i64,
        SeekFrom::Current(n) => (file.position as i64).checked_add(n).unwrap_or(-1),
        SeekFrom::End(n) => (size as i64).checked_add(n).unwrap_or(-1),
    };
    if new_pos < 0 { return SyscallError::InvalidArgument.to_u64(); }
    file.position = (new_pos as usize).min(size);
    file.position as u64
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Stat {
    pub file_type: u64,
    pub size: u64,
    pub mtime: u64,
}

pub fn fstat(table: &FdTable, fd: u32, stat: &mut Stat) -> bool {
    match table.get(fd) {
        Some(Descriptor::File(file)) => {
            stat.file_type = FileType::File as u64;
            stat.size = file_cache::size(file.file_id);
            stat.mtime = file.mtime;
            true
        }
        Some(Descriptor::PipeRead(..) | Descriptor::PipeWrite(..)) => { stat.file_type = FileType::Pipe as u64; true }
        Some(Descriptor::Keyboard) => { stat.file_type = FileType::Keyboard as u64; true }
        Some(Descriptor::Mouse) => { stat.file_type = FileType::Mouse as u64; true }
        Some(Descriptor::SerialConsole) => { stat.file_type = FileType::Serial as u64; true }
        Some(Descriptor::Framebuffer(_)) => { stat.file_type = FileType::Framebuffer as u64; true }
        Some(Descriptor::TtyRead(_) | Descriptor::TtyWrite(_)) => { stat.file_type = FileType::Tty as u64; true }
        Some(Descriptor::Socket { .. }) => { stat.file_type = FileType::Socket as u64; true }
        Some(Descriptor::Nic(_)) => { stat.file_type = FileType::Nic as u64; true }
        Some(Descriptor::Audio(_)) => { stat.file_type = FileType::Unknown as u64; true }
        Some(Descriptor::Listener(_)) => { stat.file_type = FileType::Pipe as u64; true }
        Some(Descriptor::IoUring(_)) => { stat.file_type = FileType::Unknown as u64; true }
        None => false,
    }
}

pub fn fsync(table: &mut FdTable, vfs: &mut Vfs, fd: u32) -> u64 {
    let Some(Descriptor::File(file)) = table.get_mut(fd) else {
        return SyscallError::NotFound.to_u64();
    };
    if file.modified {
        if let Err(_) = vfs.flush_file(&file.path, file.file_id, file.mtime) {
            return SyscallError::Unknown.to_u64();
        }
        file.modified = false;
    }
    0
}

pub fn ftruncate(table: &mut FdTable, fd: u32, size: u64) -> u64 {
    let Some(Descriptor::File(file)) = table.get_mut(fd) else {
        return SyscallError::NotFound.to_u64();
    };
    if !file.writable { return SyscallError::PermissionDenied.to_u64(); }
    file_cache::set_size(file.file_id, size);
    if file.position > size as usize { file.position = size as usize; }
    file.modified = true;
    file.mtime = crate::clock::nanos_since_boot();
    0
}

// ---------------------------------------------------------------------------
// Poll helpers
// ---------------------------------------------------------------------------

pub fn has_data(table: &FdTable, fd: u32) -> bool {
    match table.get(fd) {
        Some(desc) => match desc.pipe_id_read() {
            Some(id) => pipe::has_data(id),
            None => match desc {
                Descriptor::Keyboard => keyboard::has_data(),
                Descriptor::Mouse => mouse::has_data(),
                Descriptor::Listener(name) => listener::has_pending(name),
                Descriptor::SerialConsole => serial::has_data(),
                Descriptor::Nic(_) => crate::net::has_packet(),
                Descriptor::File(_) | Descriptor::Framebuffer(_) | Descriptor::Audio(_) => true,
                _ => false,
            }
        }
        None => false,
    }
}

pub fn has_space(table: &FdTable, fd: u32) -> bool {
    match table.get(fd) {
        Some(desc) => match desc.pipe_id_write() {
            Some(id) => pipe::has_space(id),
            None => matches!(desc, Descriptor::File(_) | Descriptor::SerialConsole),
        }
        None => false,
    }
}

// ---------------------------------------------------------------------------
// TTY marking
// ---------------------------------------------------------------------------

pub fn mark_tty(table: &mut FdTable, fd: u32) -> u64 {
    let Some(desc) = table.remove(fd) else {
        return SyscallError::NotFound.to_u64();
    };
    let new = match desc {
        Descriptor::PipeRead(r, _mapping) => Descriptor::TtyRead(r),
        Descriptor::PipeWrite(w, _mapping) => Descriptor::TtyWrite(w),
        Descriptor::TtyRead(_) | Descriptor::TtyWrite(_) => desc,
        other => { table.insert_at(fd, other); return SyscallError::InvalidArgument.to_u64(); }
    };
    table.insert_at(fd, new);
    0
}
