use alloc::string::String;

use crate::file_cache::{self, FileId};
use crate::id_map::IdMap;
use crate::process::Pid;
use crate::vfs::Vfs;
use crate::{device, keyboard, listener, mouse, pipe};
use crate::pipe::{PipeId, PipeReader, PipeWriter};
use crate::drivers::serial;
pub use toyos_abi::FramebufferInfo;
use toyos_abi::syscall::{FileType, OpenFlags, SeekFrom, SyscallError};

pub struct OpenFile {
    path: String,
    file_id: FileId,
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
            position: self.position,
            writable: self.writable,
            modified: false, // cloned fd starts unmodified
            mtime: self.mtime,
        }
    }
}

pub enum Descriptor {
    File(OpenFile),
    PipeRead(PipeReader),
    PipeWrite(PipeWriter),
    TtyRead(PipeReader),
    TtyWrite(PipeWriter),
    Keyboard,
    Mouse,
    SerialConsole,
    Framebuffer(FramebufferInfo),
    /// An accepted or connected IPC channel. `peer` is the process on the
    /// other end — recorded because holding this descriptor is what
    /// authorizes `SYS_PIPE_OPEN` on a pipe that process created.
    Socket { rx: PipeReader, tx: PipeWriter, peer: Pid },
    Nic(crate::net::NicInfo),
    Audio { info: toyos_abi::audio::AudioInfo, info_read: bool },
    /// A registered service. Identified by `ListenerId` and not by name: ids
    /// are never reused, so a descriptor that outlived its listener names
    /// nothing, where a name would re-resolve to whichever process registered
    /// it next. See `listener::remove`.
    Listener(listener::ListenerId),
    IoUring(crate::io_uring::RingId),
}

impl Clone for Descriptor {
    fn clone(&self) -> Self {
        match self {
            Self::PipeRead(r) => Self::PipeRead(r.clone()),
            Self::PipeWrite(w) => Self::PipeWrite(w.clone()),
            Self::TtyRead(r) => Self::TtyRead(r.clone()),
            Self::TtyWrite(w) => Self::TtyWrite(w.clone()),
            Self::Socket { rx, tx, peer } => Self::Socket { rx: rx.clone(), tx: tx.clone(), peer: *peer },
            Self::File(file) => Self::File(file.clone()),
            Self::Keyboard => Self::Keyboard,
            Self::Mouse => Self::Mouse,
            Self::SerialConsole => Self::SerialConsole,
            Self::Framebuffer(info) => Self::Framebuffer(*info),
            Self::Nic(info) => Self::Nic(*info),
            Self::Audio { info, info_read } => Self::Audio { info: *info, info_read: *info_read },
            Self::Listener(id) => Self::Listener(*id),
            Self::IoUring(id) => Self::IoUring(*id),
        }
    }
}

impl Descriptor {
    pub fn pipe_id_read(&self) -> Option<PipeId> {
        match self {
            Self::PipeRead(r) | Self::TtyRead(r) => Some(r.id()),
            Self::Socket { rx, .. } => Some(rx.id()),
            _ => None,
        }
    }

    pub fn pipe_id_write(&self) -> Option<PipeId> {
        match self {
            Self::PipeWrite(w) | Self::TtyWrite(w) => Some(w.id()),
            Self::Socket { tx, .. } => Some(tx.id()),
            _ => None,
        }
    }

    pub fn read_source(&self) -> Option<crate::io_uring::Source> {
        use crate::io_uring::Source;
        match self {
            Self::Keyboard => Some(Source::Keyboard),
            Self::Mouse => Some(Source::Mouse),
            Self::SerialConsole => Some(Source::Keyboard),
            Self::Nic(_) => Some(Source::Network),
            Self::Listener(id) => Some(Source::Listener(*id)),
            Self::PipeRead(r) | Self::TtyRead(r) => Some(Source::PipeReadable(r.id())),
            Self::Socket { rx, .. } => Some(Source::PipeReadable(rx.id())),
            Self::Audio { .. } => Some(Source::Audio),
            Self::File(_) | Self::Framebuffer(_) => None,
            Self::PipeWrite(..) | Self::TtyWrite(_) => None,
            Self::IoUring(_) => None,
        }
    }

    pub fn write_source(&self) -> Option<crate::io_uring::Source> {
        use crate::io_uring::Source;
        match self {
            Self::PipeWrite(w) | Self::TtyWrite(w) => Some(Source::PipeWritable(w.id())),
            Self::Socket { tx, .. } => Some(Source::PipeWritable(tx.id())),
            Self::File(_) | Self::SerialConsole => None,
            Self::Keyboard | Self::Mouse | Self::Nic(_) | Self::Audio { .. }
            | Self::Framebuffer(_) | Self::Listener(_)
            | Self::PipeRead(..) | Self::TtyRead(_) | Self::IoUring(_) => None,
        }
    }
}

const MAX_FDS: usize = 1024;

/// A process's descriptor table.
///
/// A newtype around `IdMap` rather than an alias for it, so that `MAX_FDS` can
/// live on the insert primitives instead of at the call sites. The inner map
/// is private: every path that can grow a process's table — open, accept,
/// dup, dup2, a spawn fd_map — goes through `insert` or `insert_at` and gets
/// the cap, and there is no third way in to forget it.
pub struct FdTable {
    map: IdMap<u32, Descriptor>,
}

impl FdTable {
    pub fn new() -> Self {
        Self { map: IdMap::new() }
    }

    /// Insert at the lowest unused id.
    pub fn insert(&mut self, desc: Descriptor) -> Result<u32, SyscallError> {
        self.check_room(None)?;
        Ok(self.map.insert(desc))
    }

    /// Insert at a caller-chosen id, replacing whatever is there.
    pub fn insert_at(&mut self, fd: u32, desc: Descriptor) -> Result<(), SyscallError> {
        self.check_room(Some(fd))?;
        self.map.insert_at(fd, desc);
        Ok(())
    }

    /// Replace the descriptor at `fd` with a function of itself.
    ///
    /// The length is unchanged by construction, which is why this is the only
    /// operation allowed to reach the uncapped `IdMap::insert_at`.
    pub fn update(&mut self, fd: u32, f: impl FnOnce(Descriptor) -> Descriptor) {
        let Some(desc) = self.map.remove(fd) else { return };
        self.map.insert_at(fd, f(desc));
    }

    /// Refuse growth past `MAX_FDS`. Overwriting a live id is not growth.
    fn check_room(&self, replacing: Option<u32>) -> Result<(), SyscallError> {
        if replacing.is_some_and(|fd| self.map.get(fd).is_some()) {
            return Ok(());
        }
        if self.map.len() >= MAX_FDS {
            return Err(SyscallError::ResourceExhausted);
        }
        Ok(())
    }

    pub fn get(&self, fd: u32) -> Option<&Descriptor> {
        self.map.get(fd)
    }

    pub fn get_mut(&mut self, fd: u32) -> Option<&mut Descriptor> {
        self.map.get_mut(fd)
    }

    pub fn remove(&mut self, fd: u32) -> Option<Descriptor> {
        self.map.remove(fd)
    }

    pub fn drain(&mut self) -> impl Iterator<Item = (u32, Descriptor)> + '_ {
        self.map.drain()
    }

    pub fn iter(&self) -> impl Iterator<Item = (u32, &Descriptor)> {
        self.map.iter()
    }

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

    if truncate && create {
        let mtime = crate::clock::nanos_since_boot();
        vfs.delete(path);
        let file_id = match vfs.create_file(path, mtime) {
            Ok(id) => id,
            Err(_) => return SyscallError::Unknown.to_u64(),
        };
        let file = OpenFile {
            path: String::from(path),
            file_id,
            position: 0,
            writable,
            modified: false,
            mtime,
        };
        return match table.insert(Descriptor::File(file)) {
            Ok(fd) => fd as u64,
            Err(e) => e.to_u64(),
        };
    }

    match vfs.open_file(path) {
        Some(file_id) => {
            let mtime = vfs.file_mtime(path);
            let size = file_cache::size(file_id);
            let position = if append { size as usize } else { 0 };
            let file = OpenFile {
                path: String::from(path),
                file_id,
                position,
                writable,
                modified: false,
                mtime,
            };
            match table.insert(Descriptor::File(file)) {
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
                    position: 0,
                    writable,
                    modified: false,
                    mtime,
                };
                match table.insert(Descriptor::File(file)) {
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
    let sources = [desc.read_source(), desc.write_source()];
    if sources.iter().any(|s| s.is_some()) {
        crate::io_uring::remove_fd(&sources);
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
        Descriptor::Keyboard | Descriptor::Mouse | Descriptor::Framebuffer(_) | Descriptor::Nic(_) | Descriptor::Audio { .. } => {
            device::release_descriptor(&desc, pid);
        }
        Descriptor::Listener(id) => {
            listener::remove(*id);
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
            Descriptor::Keyboard | Descriptor::Mouse | Descriptor::Framebuffer(_) | Descriptor::Nic(_) | Descriptor::Audio { .. } => {
                device::release_descriptor(&desc, pid);
            }
            Descriptor::Listener(id) => {
                listener::remove(*id);
            }
            Descriptor::IoUring(id) => {
                crate::io_uring::destroy(*id);
            }
            _ => {}
        }
    }
}

// Read / Write / Seek / Stat

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
                );
                read += to_read;
            }
            file.position += count;
            Some(count as u64)
        }
        Descriptor::PipeRead(r) | Descriptor::TtyRead(r) => {
            pipe::try_read(r.id(), buf).map(|n| n as u64)
        }
        Descriptor::Socket { rx, .. } => {
            pipe::try_read(rx.id(), buf).map(|n| n as u64)
        }
        // **A read of an input descriptor reads the queue and drives no
        // hardware.** Both of these called `xhci::poll_if_pending()` first,
        // which made whichever thread happened to read the mouse into the
        // driver's enumeration and recovery engine: it takes `XHCI` — a ticket
        // spinlock, so preemption off for its whole life — and inside it a
        // hot-plug enumerates a device and a broken endpoint runs a recovery,
        // each of which spins on deadlines measured in seconds. On the T14 that
        // was the compositor's own mouse read, and the desktop froze for
        // multi-second stretches with a live kernel and nothing dropped.
        //
        // Nothing is lost by removing it: `drain_irqs` calls the same function
        // at the top of every scheduler pass, so a report is dispatched and its
        // waiters woken before any thread that wants it is picked. What a
        // reader gives up is at most one pass of latency; what it gains is that
        // a read cannot become a bus operation.
        Descriptor::Keyboard => {
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
        Descriptor::Audio { info, info_read } => {
            if !*info_read {
                let bytes = info.as_bytes();
                let count = buf.len().min(bytes.len());
                buf[..count].copy_from_slice(&bytes[..count]);
                *info_read = true;
                return Some(count as u64);
            }
            if buf.len() < toyos_abi::audio::AudioCompletionRecord::SIZE {
                return Some(SyscallError::InvalidArgument.to_u64());
            }
            // Completion records, oldest first. Empty → None: blocking reads
            // park on `waitqs::AUDIO`, nonblocking reads get WouldBlock.
            let n = crate::audio::drain_completed(buf);
            if n == 0 { None } else { Some(n as u64) }
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
            let mut written = 0;
            let mut refused = false;
            while written < buf.len() {
                let abs_pos = file.position + written;
                let page_idx = (abs_pos / 4096) as u32;
                let offset_in_page = abs_pos % 4096;
                let remaining_in_page = 4096 - offset_in_page;
                let to_write = remaining_in_page.min(buf.len() - written);
                // A partial write whose page could not be re-read off the
                // device is refused rather than merged into zeros, so this
                // stops short instead of claiming bytes that are not in the
                // file. Short counts are what `write` means; before this the
                // return was `buf.len()` unconditionally.
                if file_cache::write_page(
                    file.file_id,
                    page_idx,
                    offset_in_page,
                    &buf[written..written + to_write],
                )
                .is_err()
                {
                    refused = true;
                    break;
                }
                written += to_write;
            }
            if written == 0 && refused {
                // The honest code does not exist: none of `SyscallError`'s nine
                // variants means "the device did not do it", and adding one is
                // an ABI change that needs discussing. `Unknown` is the only
                // one that does not claim something false. Known issues carries
                // the conversion; this is its first call site.
                return Some(SyscallError::Unknown.to_u64());
            }
            file.position += written;
            file.modified = true;
            file.mtime = crate::clock::nanos_since_boot();
            Some(written as u64)
        }
        Descriptor::PipeWrite(w) | Descriptor::TtyWrite(w) => {
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
            serial::SerialWriter::console().write_bytes(buf);
            Some(buf.len() as u64)
        }
        Descriptor::Audio { .. } => {
            if !buf.is_empty() {
                match buf[0] {
                    0 => crate::audio::stop(),
                    1 => crate::audio::start(),
                    _ => {}
                }
                Some(1)
            } else {
                Some(0)
            }
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
        Some(Descriptor::Audio { .. }) => { stat.file_type = FileType::Unknown as u64; true }
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

pub fn has_data(table: &FdTable, fd: u32) -> bool {
    match table.get(fd) {
        Some(desc) => match desc.pipe_id_read() {
            Some(id) => pipe::has_data(id),
            None => match desc {
                Descriptor::Keyboard => keyboard::has_data(),
                Descriptor::Mouse => mouse::has_data(),
                Descriptor::Listener(id) => listener::has_pending_by_id(*id),
                Descriptor::SerialConsole => serial::has_data(),
                Descriptor::Nic(_) => crate::net::has_packet(),
                Descriptor::Audio { info_read: false, .. } => true,
                Descriptor::Audio { info_read: true, .. } => crate::audio::has_pending(),
                Descriptor::File(_) | Descriptor::Framebuffer(_) => true,
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

pub fn mark_tty(table: &mut FdTable, fd: u32) -> u64 {
    match table.get(fd) {
        Some(Descriptor::PipeRead(_) | Descriptor::PipeWrite(_)) => {}
        Some(Descriptor::TtyRead(_) | Descriptor::TtyWrite(_)) => return 0,
        Some(_) => return SyscallError::InvalidArgument.to_u64(),
        None => return SyscallError::NotFound.to_u64(),
    }
    // Moves the PipeReader/PipeWriter into the Tty variant — no clone, so the
    // pipe refcount is untouched.
    table.update(fd, |desc| match desc {
        Descriptor::PipeRead(r) => Descriptor::TtyRead(r),
        Descriptor::PipeWrite(w) => Descriptor::TtyWrite(w),
        other => other,
    });
    0
}
